use evm_adapters::sputnik::cheatcodes::CONSOLE_ABI;
use evm_adapters::sputnik::cheatcodes::HEVMCONSOLE_ABI;
use evm_adapters::sputnik::cheatcodes::HEVM_ABI;
use crate::cmd::{build::BuildArgs, compile, manual_compile, Cmd};
use clap::{Parser, ValueHint};

use forge::ContractRunner;
use foundry_utils::IntoFunction;
use std::{collections::BTreeMap, path::PathBuf};
use ui::{TUIExitReason, Tui, Ui};

use ethers::solc::{MinimalCombinedArtifacts, Project};

use crate::opts::evm::EvmArgs;
use ansi_term::Colour;
use ethers::{
    abi::Abi,
    solc::artifacts::{
        BytecodeObject, CompactContractBytecode,
        ContractBytecode, ContractBytecodeSome,
    },
    types::U256,
};
use evm_adapters::{
    call_tracing::ExecutionInfo,
    evm_opts::{BackendKind, EvmOpts},
    sputnik::{cheatcodes::debugger::DebugArena, helpers::vm},
};
use foundry_config::{figment::Figment, Config};

// Loads project's figment and merges the build cli arguments into it
foundry_config::impl_figment_convert!(RunArgs, opts, evm_opts);

#[derive(Debug, Clone, Parser)]
pub struct RunArgs {
    #[clap(help = "the path to the contract to run", value_hint = ValueHint::FilePath)]
    pub path: PathBuf,

    #[clap(flatten)]
    pub evm_opts: EvmArgs,

    #[clap(flatten)]
    opts: BuildArgs,

    #[clap(
        long,
        short,
        help = "the contract you want to call and deploy, only necessary if there are more than 1 contract (Interfaces do not count) definitions on the script"
    )]
    pub target_contract: Option<String>,

    #[clap(
        long,
        short,
        help = "the function you want to call on the script contract, defaults to run()"
    )]
    pub sig: Option<String>,
}

impl Cmd for RunArgs {
    type Output = ();
    fn run(self) -> eyre::Result<Self::Output> {
        // Keeping it like this for simplicity.
        #[cfg(not(feature = "sputnik-evm"))]
        unimplemented!("`run` does not work with EVMs other than Sputnik yet");

        let figment: Figment = From::from(&self);
        let mut evm_opts = figment.extract::<EvmOpts>()?;
        let config = Config::from_provider(figment).sanitized();
        let evm_version = config.evm_version;
        if evm_opts.debug {
            evm_opts.verbosity = 3;
        }

        let func = IntoFunction::into(self.sig.as_deref().unwrap_or("run()"));
        let BuildOutput {
            project,
            contract,
            highlevel_known_contracts,
            sources,
            predeploy_libraries,
        } = self.build(config, &evm_opts)?;

        let mut known_contracts = highlevel_known_contracts
            .iter()
            .map(|(name, c)| {
                (
                    name.clone(),
                    (
                        c.abi.clone(),
                        c.deployed_bytecode.clone().into_bytes().expect("not bytecode").to_vec(),
                    ),
                )
            })
            .collect::<BTreeMap<String, (Abi, Vec<u8>)>>();

        known_contracts.insert("VM".to_string(), (HEVM_ABI.clone(), Vec::new()));
        known_contracts.insert("VM_CONSOLE".to_string(), (HEVMCONSOLE_ABI.clone(), Vec::new()));
        known_contracts.insert("CONSOLE".to_string(), (CONSOLE_ABI.clone(), Vec::new()));

        let CompactContractBytecode { abi, bytecode, .. } = contract;
        let abi = abi.expect("No abi for contract");
        let bytecode = bytecode.expect("No bytecode").object.into_bytes().unwrap();
        let needs_setup = abi.functions().any(|func| func.name == "setUp");

        let mut cfg = crate::utils::sputnik_cfg(&evm_version);
        cfg.create_contract_limit = None;
        let vicinity = evm_opts.vicinity()?;
        let backend = evm_opts.backend(&vicinity)?;

        // need to match on the backend type
        let result = match backend {
            BackendKind::Simple(ref backend) => {
                let runner = ContractRunner::new(
                    &evm_opts,
                    &cfg,
                    backend,
                    &abi,
                    bytecode,
                    Some(evm_opts.sender),
                    None,
                    predeploy_libraries,
                );
                runner.run_test(&func, needs_setup, Some(&known_contracts))?
            }
            BackendKind::Shared(ref backend) => {
                let runner = ContractRunner::new(
                    &evm_opts,
                    &cfg,
                    backend,
                    &abi,
                    bytecode,
                    Some(evm_opts.sender),
                    None,
                    predeploy_libraries,
                );
                runner.run_test(&func, needs_setup, Some(&known_contracts))?
            }
        };

        if evm_opts.debug {
            // 4. Boot up debugger
            let source_code: BTreeMap<u32, String> = sources
                .iter()
                .map(|(id, path)| {
                    if let Some(resolved) =
                        project.paths.resolve_library_import(&PathBuf::from(path))
                    {
                        (
                            *id,
                            std::fs::read_to_string(resolved).expect(&*format!(
                                "Something went wrong reading the source file: {:?}",
                                path
                            )),
                        )
                    } else {
                        (
                            *id,
                            std::fs::read_to_string(path).expect(&*format!(
                                "Something went wrong reading the source file: {:?}",
                                path
                            )),
                        )
                    }
                })
                .collect();

            let calls: Vec<DebugArena> = result.debug_calls.expect("Debug must be enabled by now");
            println!("debugging");
            let index = if needs_setup && calls.len() > 1 { 1 } else { 0 };
            let mut flattened = Vec::new();
            calls[index].flatten(0, &mut flattened);
            flattened = flattened[1..].to_vec();
            let tui = Tui::new(
                flattened,
                0,
                result.identified_contracts.expect("debug but not verbosity"),
                highlevel_known_contracts,
                source_code,
            )?;
            match tui.start().expect("Failed to start tui") {
                TUIExitReason::CharExit => return Ok(()),
            }
        } else if evm_opts.verbosity > 2 {
            // support traces
            if let (Some(traces), Some(identified_contracts)) =
                (&result.traces, &result.identified_contracts)
            {
                if !result.success && evm_opts.verbosity == 3 || evm_opts.verbosity > 3 {
                    let mut ident = identified_contracts.clone();
                    let (funcs, events, errors) =
                        foundry_utils::flatten_known_contracts(&known_contracts);
                    let mut exec_info =
                        ExecutionInfo::new(&known_contracts, &mut ident, &funcs, &events, &errors);
                    let vm = vm();
                    if evm_opts.verbosity > 4 || !result.success {
                        // print setup calls as well
                        traces.iter().for_each(|trace| {
                            trace.pretty_print(0, &mut exec_info, &vm, "");
                        });
                    } else if !traces.is_empty() {
                        traces.last().expect("no last but not empty").pretty_print(
                            0,
                            &mut exec_info,
                            &vm,
                            "",
                        );
                    }
                } else {
                    // 5. print the result nicely
                    if result.success {
                        println!("{}", Colour::Green.paint("Script ran successfully."));
                    } else {
                        println!("{}", Colour::Red.paint("Script failed."));
                    }

                    println!("Gas Used: {}", result.gas_used);
                    println!("== Logs == ");
                    result.logs.iter().for_each(|log| println!("{}", log));
                }
                println!();
            } else if result.traces.is_none() {
                eyre::bail!("Unexpected error: No traces despite verbosity level. Please report this as a bug");
            } else if result.identified_contracts.is_none() {
                eyre::bail!("Unexpected error: No identified contracts. Please report this as a bug");
            }
        } else {
            // 5. print the result nicely
            if result.success {
                println!("{}", Colour::Green.paint("Script ran successfully."));
            } else {
                println!("{}", Colour::Red.paint("Script failed."));
            }

            println!("Gas Used: {}", result.gas_used);
            println!("== Logs == ");
            result.logs.iter().for_each(|log| println!("{}", log));
        }

        Ok(())
    }
}

pub struct BuildOutput {
    pub project: Project<MinimalCombinedArtifacts>,
    pub contract: CompactContractBytecode,
    pub highlevel_known_contracts: BTreeMap<String, ContractBytecodeSome>,
    pub sources: BTreeMap<u32, String>,
    pub predeploy_libraries: Vec<ethers::types::Bytes>,
}

impl RunArgs {
    /// Compiles the file with auto-detection and compiler params.
    pub fn build(&self, config: Config, evm_opts: &EvmOpts) -> eyre::Result<BuildOutput> {
        let target_contract = dunce::canonicalize(&self.path)?;
        let (project, output) = if let Ok(mut project) = config.project() {
            // TODO: caching causes no output until https://github.com/gakonst/ethers-rs/issues/727
            // is fixed
            project.cached = false;
            project.no_artifacts = true;

            // target contract may not be in the compilation path, add it and manually compile
            match manual_compile(&project, vec![target_contract]) {
                Ok(output) => (project, output),
                Err(e) => {
                    println!("No extra contracts compiled {:?}", e);
                    let mut target_project = config.ephemeral_no_artifacts_project()?;
                    target_project.cached = false;
                    target_project.no_artifacts = true;
                    let res = compile(&target_project)?;
                    (target_project, res)
                }
            }
        } else {
            let mut target_project = config.ephemeral_no_artifacts_project()?;
            target_project.cached = false;
            target_project.no_artifacts = true;
            let res = compile(&target_project)?;
            (target_project, res)
        };
        println!("success.");

        let (sources, all_contracts) = output.output().split();

        let mut contracts: BTreeMap<String, CompactContractBytecode> = BTreeMap::new();
        all_contracts.0.iter().for_each(|(source, output_contracts)| {
            contracts.extend(
                output_contracts
                    .iter()
                    .map(|(n, c)| (source.to_string() + ":" + n, c.clone().into()))
                    .collect::<BTreeMap<String, CompactContractBytecode>>(),
            );
        });

        // create a mapping of fname => Vec<(fname, file, key)>,
        let link_tree: BTreeMap<String, Vec<(String, String, String)>> = contracts
            .iter()
            .map(|(fname, contract)| {
                (
                    fname.to_string(),
                    contract
                        .all_link_references()
                        .iter()
                        .flat_map(|(file, link)| {
                            link.keys().map(|key| {
                                (file.to_string() + ":" + key, file.to_string(), key.to_string())
                            })
                        })
                        .collect::<Vec<(String, String, String)>>(),
                )
            })
            .collect();

        // grab the nonce, either from the rpc node or start from 1
        let nonce = if let Some(url) = &evm_opts.fork_url {
            foundry_utils::next_nonce(
                evm_opts.sender,
                url,
                evm_opts.fork_block_number.map(Into::into),
            )
            .unwrap_or_default() +
                1
        } else {
            U256::one()
        };

        let mut run_dependencies = vec![];
        let mut contract =
            CompactContractBytecode { abi: None, bytecode: None, deployed_bytecode: None };
        let mut highlevel_known_contracts = BTreeMap::new();

        let mut target_fname = std::fs::canonicalize(self.path.clone())
            .expect("Couldn't convert contract path to absolute path")
            .to_str()
            .expect("Bad path to string")
            .to_string();

        let mut no_target_name = true;
        let mut matched = false;
        if let Some(target_name) = &self.target_contract {
            target_fname = target_fname + ":" + target_name;
            no_target_name = false;
        }
        for fname in contracts.keys() {
            let (abi, maybe_deployment_bytes, maybe_runtime) = if let Some(c) = contracts.get(fname)
            {
                (c.abi.as_ref(), c.bytecode.as_ref(), c.deployed_bytecode.as_ref())
            } else {
                (None, None, None)
            };
            if let (Some(abi), Some(bytecode), Some(runtime)) =
                (abi, maybe_deployment_bytes, maybe_runtime)
            {
                // we are going to mutate, but library contract addresses may change based on
                // the test so we clone
                //
                // TODO: verify the above statement. Maybe not? and we can modify in place.
                let mut target_bytecode = bytecode.clone();
                let mut rt = runtime.clone();
                let mut target_bytecode_runtime = rt.bytecode.expect("No target runtime").clone();

                // instantiate a vector that gets filled with library deployment bytecode
                let mut dependencies = vec![];

                match bytecode.object {
                    BytecodeObject::Unlinked(_) => {
                        // link needed
                        foundry_utils::recurse_link(
                            fname.to_string(),
                            (&mut target_bytecode, &mut target_bytecode_runtime),
                            &contracts,
                            &link_tree,
                            &mut dependencies,
                            nonce,
                            evm_opts.sender,
                        );
                    }
                    BytecodeObject::Bytecode(ref bytes) => {
                        if bytes.as_ref().is_empty() {
                            // abstract, skip
                            continue
                        }
                    }
                }

                rt.bytecode = Some(target_bytecode_runtime);
                let tc = CompactContractBytecode {
                    abi: Some(abi.clone()),
                    bytecode: Some(target_bytecode),
                    deployed_bytecode: Some(rt),
                };

                let split = fname.split(':').collect::<Vec<&str>>();

                // if its the target contract, grab the info
                if no_target_name {
                    if split[0] == target_fname {
                        if matched {
                            eyre::bail!("Multiple contracts in the target path. Please specify the contract name with `-t ContractName`")
                        }
                        run_dependencies = dependencies.clone();
                        contract = tc.clone();
                        matched = true;
                    }
                } else if &target_fname == fname {
                    run_dependencies = dependencies.clone();
                    contract = tc.clone();
                    matched = true;
                }

                let tc: ContractBytecode = tc.into();
                let contract_name = if split.len() > 1 { split[1] } else { split[0] };
                highlevel_known_contracts.insert(contract_name.to_string(), tc.unwrap());
            }
        }

        Ok(BuildOutput {
            project,
            contract,
            highlevel_known_contracts,
            sources: sources.into_ids().collect(),
            predeploy_libraries: run_dependencies,
        })
    }
}
