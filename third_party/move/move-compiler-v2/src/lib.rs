// Copyright © Aptos Foundation
// Parts of the project are originally copyright © Meta Platforms, Inc.
// SPDX-License-Identifier: Apache-2.0

mod bytecode_generator;
mod experiments;
mod file_format_generator;
mod options;
pub mod pipeline;

use crate::pipeline::{
    livevar_analysis_processor::LiveVarAnalysisProcessor, visibility_checker::VisibilityChecker,
};
use anyhow::bail;
use codespan::Span;
use codespan_reporting::term::termcolor::{ColorChoice, StandardStream, WriteColor};
pub use experiments::*;
use move_command_line_common::files::FileHash;
use move_compiler::{
    compiled_unit::{
        AnnotatedCompiledModule, AnnotatedCompiledScript, AnnotatedCompiledUnit, CompiledUnit,
        FunctionInfo,
    },
    diagnostics::FilesSourceText,
    shared::{known_attributes::KnownAttribute, unique_map::UniqueMap},
};
use move_ir_types::location::Loc;
use move_model::{model::GlobalEnv, PackageInfo};
use move_stackless_bytecode::function_target_pipeline::{
    FunctionTargetPipeline, FunctionTargetsHolder, FunctionVariant,
};
use move_symbol_pool::Symbol;
pub use options::*;
use std::{collections::BTreeSet, path::Path};

/// Run Move compiler and print errors to stderr.
pub fn run_move_compiler_to_stderr(
    options: Options,
) -> anyhow::Result<(GlobalEnv, Vec<AnnotatedCompiledUnit>)> {
    let mut error_writer = StandardStream::stderr(ColorChoice::Auto);
    run_move_compiler(&mut error_writer, options)
}

/// Run move compiler and print errors to given writer.
pub fn run_move_compiler(
    error_writer: &mut impl WriteColor,
    options: Options,
) -> anyhow::Result<(GlobalEnv, Vec<AnnotatedCompiledUnit>)> {
    // Run context check.
    let env = run_checker(options.clone())?;
    check_errors(&env, error_writer, "checking errors")?;
    // Run code generator
    let mut targets = run_bytecode_gen(&env);
    check_errors(&env, error_writer, "code generation errors")?;
    // Run transformation pipeline
    let pipeline = bytecode_pipeline(&env);
    if options.dump_bytecode {
        // Dump bytecode to files, using a basename for the individual sources derived
        // from the first input file.
        let dump_base_name = options
            .sources
            .get(0)
            .and_then(|f| {
                Path::new(f)
                    .file_name()
                    .map(|f| f.to_string_lossy().as_ref().to_owned())
            })
            .unwrap_or_else(|| "dump".to_owned());
        pipeline.run_with_dump(&env, &mut targets, &dump_base_name, false)
    } else {
        pipeline.run(&env, &mut targets)
    }
    check_errors(&env, error_writer, "stackless-bytecode analysis errors")?;
    let modules_and_scripts = run_file_format_gen(&env, &targets);
    check_errors(&env, error_writer, "assembling errors")?;
    let annotated = annotate_units(&env, modules_and_scripts);
    Ok((env, annotated))
}

/// Run the type checker and return the global env (with errors if encountered). The result
/// fails not on context checking errors, but possibly on i/o errors.
pub fn run_checker(options: Options) -> anyhow::Result<GlobalEnv> {
    // Run the model builder, which performs context checking.
    let addrs = move_model::parse_addresses_from_options(options.named_address_mapping.clone())?;
    let mut env = move_model::run_model_builder_in_compiler_mode(
        PackageInfo {
            sources: options.sources.clone(),
            address_map: addrs.clone(),
        },
        vec![PackageInfo {
            sources: options.dependencies.clone(),
            address_map: addrs.clone(),
        }],
        options.skip_attribute_checks,
        if !options.skip_attribute_checks && options.known_attributes.is_empty() {
            KnownAttribute::get_all_attribute_names()
        } else {
            &options.known_attributes
        },
    )?;
    // Store address aliases
    let map = addrs
        .into_iter()
        .map(|(s, a)| (env.symbol_pool().make(&s), a.into_inner()))
        .collect();
    env.set_address_alias_map(map);
    // Store options in env, for later access
    env.set_extension(options);
    Ok(env)
}

// Run the (stackless) bytecode generator. For each function which is target of the
// compilation, create an entry in the functions target holder which encapsulate info
// like the generated bytecode.
pub fn run_bytecode_gen(env: &GlobalEnv) -> FunctionTargetsHolder {
    let mut targets = FunctionTargetsHolder::default();
    let mut todo = BTreeSet::new();
    let mut done = BTreeSet::new();
    for module in env.get_modules() {
        if module.is_target() {
            for fun in module.get_functions() {
                let id = fun.get_qualified_id();
                todo.insert(id);
            }
        }
    }
    while let Some(id) = todo.pop_first() {
        done.insert(id);
        let data = bytecode_generator::generate_bytecode(env, id);
        targets.insert_target_data(&id, FunctionVariant::Baseline, data);
        for callee in env
            .get_function(id)
            .get_called_functions()
            .expect("called functions available")
        {
            if !done.contains(callee) {
                todo.insert(*callee);
            }
        }
    }
    targets
}

pub fn run_file_format_gen(env: &GlobalEnv, targets: &FunctionTargetsHolder) -> Vec<CompiledUnit> {
    file_format_generator::generate_file_format(env, targets)
}

/// Returns the bytecode processing pipeline.
pub fn bytecode_pipeline(_env: &GlobalEnv) -> FunctionTargetPipeline {
    let mut pipeline = FunctionTargetPipeline::default();
    pipeline.add_processor(Box::new(LiveVarAnalysisProcessor()));
    pipeline.add_processor(Box::new(VisibilityChecker()));
    pipeline
}

/// Report any diags in the env to the writer and fail if there are errors.
pub fn check_errors<W: WriteColor>(
    env: &GlobalEnv,
    error_writer: &mut W,
    msg: &'static str,
) -> anyhow::Result<()> {
    let options = env.get_extension::<Options>().unwrap_or_default();
    env.report_diag(error_writer, options.report_severity());
    if env.has_errors() {
        bail!("exiting with {}", msg);
    } else {
        Ok(())
    }
}

/// Annotate the given compiled units.
/// TODO: this currently only fills in defaults. The annotations are only used in
/// the prover, and compiler v2 is not yet connected to the prover.
pub fn annotate_units(env: &GlobalEnv, units: Vec<CompiledUnit>) -> Vec<AnnotatedCompiledUnit> {
    let mut loc = env.unknown_move_ir_loc();
    units
        .into_iter()
        .map(|u| match u {
            CompiledUnit::Module(named_module) => {
                let mid = named_module.module.self_id();
                if let (Some(hash), Some(span)) = get_module_file_hash(env, mid.name().as_str()) {
                    loc = Loc::new(hash, span.start().0, span.end().0);
                }
                AnnotatedCompiledUnit::Module(AnnotatedCompiledModule {
                    loc,
                    module_name_loc: loc,
                    address_name: None,
                    named_module,
                    function_infos: UniqueMap::new(),
                })
            },
            CompiledUnit::Script(named_script) => {
                AnnotatedCompiledUnit::Script(AnnotatedCompiledScript {
                    loc,
                    named_script,
                    function_info: FunctionInfo {
                        spec_info: Default::default(),
                    },
                })
            },
        })
        .collect()
}

/// Computes the `FilesSourceText` from the global environment, which maps IR loc file hashes
/// into files and sources. This value is used for the package system only.
pub fn make_files_source_text(env: &GlobalEnv) -> FilesSourceText {
    let mut result = FilesSourceText::new();
    for fid in env.get_source_file_ids() {
        if let Some(hash) = env.get_file_hash(fid) {
            let file_name = Symbol::from(env.get_file(fid).to_string_lossy().to_string());
            let file_content = env.get_file_source(fid).to_owned();
            result.insert(hash, (file_name, file_content));
        }
    }
    result
}

fn get_module_file_hash(env: &GlobalEnv, module_name: &str) -> (Option<FileHash>, Option<Span>) {
    for m in env.get_modules() {
        let m_name = m.get_name().display(env).to_string();
        if m_name.as_str() == module_name {
            let fid = m.get_loc().file_id();
            return (env.get_file_hash(fid), Some(m.get_loc().span()));
        }
    }
    (None, None)
}
