use crate::core::compiler::{Compilation, CompileKind, Doctest, Unit};
use crate::core::shell::Verbosity;
use crate::core::{TargetKind, Workspace};
use crate::ops;
use crate::util::errors::CargoResult;
use crate::util::{add_path_args, CargoTestError, Config, Test};
use cargo_util::{ProcessBuilder, ProcessError};
use std::collections::{HashMap, VecDeque};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};

pub struct TestOptions {
    pub compile_opts: ops::CompileOptions,
    pub no_run: bool,
    pub no_fail_fast: bool,
}

pub fn run_tests(
    ws: &Workspace<'_>,
    options: &TestOptions,
    test_args: &[&str],
) -> CargoResult<Option<CargoTestError>> {
    let compilation = compile_tests(ws, options)?;

    if options.no_run {
        return Ok(None);
    }
    let (test, mut errors) = run_unit_tests(ws.config(), options, test_args, &compilation)?;

    // If we have an error and want to fail fast, then return.
    if !errors.is_empty() && !options.no_fail_fast {
        return Ok(Some(CargoTestError::new(test, errors)));
    }

    let (doctest, docerrors) = run_doc_tests(ws, options, test_args, &compilation)?;
    let test = if docerrors.is_empty() { test } else { doctest };
    errors.extend(docerrors);
    if errors.is_empty() {
        Ok(None)
    } else {
        Ok(Some(CargoTestError::new(test, errors)))
    }
}

pub fn run_benches(
    ws: &Workspace<'_>,
    options: &TestOptions,
    args: &[&str],
) -> CargoResult<Option<CargoTestError>> {
    let compilation = compile_tests(ws, options)?;

    if options.no_run {
        return Ok(None);
    }

    let mut args = args.to_vec();
    args.push("--bench");

    let (test, errors) = run_unit_tests(ws.config(), options, &args, &compilation)?;

    match errors.len() {
        0 => Ok(None),
        _ => Ok(Some(CargoTestError::new(test, errors))),
    }
}

fn compile_tests<'a>(ws: &Workspace<'a>, options: &TestOptions) -> CargoResult<Compilation<'a>> {
    let mut compilation = ops::compile(ws, &options.compile_opts)?;
    compilation.tests.sort();
    Ok(compilation)
}

fn run_unit_tests(
    config: &Config,
    options: &TestOptions,
    test_args: &[&str],
    compilation: &Compilation<'_>,
) -> CargoResult<(Test, Vec<ProcessError>)> {
    let cwd = config.cwd();

    let batches: CargoResult<Vec<_>> = compilation
        .tests
        .iter()
        .map(|unit_output| {
            let exe_display = get_exe_display(&unit_output.unit, &unit_output.path, cwd);
            let mut cmd = compilation.target_process(
                &unit_output.path,
                unit_output.unit.kind,
                &unit_output.unit.pkg,
                *&unit_output.script_meta,
            )?;

            cmd.args(test_args);
            if unit_output.unit.target.harness() && config.shell().verbosity() == Verbosity::Quiet {
                cmd.arg("--quiet");
            }

            if !test_args.contains(&"--color=never") && config.shell().err_supports_color() {
                cmd.arg("--color=always");
            }

            let cmd_display = if config.shell().verbosity() == Verbosity::Verbose {
                Some(format!("{}", cmd))
            } else {
                None
            };

            let display = TestBatchDisplay {
                exe_display,
                cmd_display,
            };

            let error_display = TestBatchErrorDisplay {
                target_kind: unit_output.unit.target.kind().clone(),
                target_name: unit_output.unit.target.name().to_string(),
                pkg_name: unit_output.unit.pkg.name().to_string(),
            };

            Ok(TestBatch {
                display,
                error_display,
                cmd,
            })
        })
        .collect();
    let batches = batches?;
    let mut errors = run_test_batches(batches, options, config)?;

    // if there is only one error then print more details about the source of that error
    if errors.len() == 1 {
        let (batch, e) = errors.pop().unwrap();
        Ok((
            Test::UnitTest {
                kind: batch.error_display.target_kind,
                name: batch.error_display.target_name,
                pkg_name: batch.error_display.pkg_name,
            },
            vec![e],
        ))
    } else {
        Ok((Test::Multiple, errors.into_iter().map(|(_, e)| e).collect()))
    }
}

fn run_doc_tests(
    ws: &Workspace<'_>,
    options: &TestOptions,
    test_args: &[&str],
    compilation: &Compilation<'_>,
) -> CargoResult<(Test, Vec<ProcessError>)> {
    let config = ws.config();

    let doctest_xcompile = config.cli_unstable().doctest_xcompile;
    let doctest_in_workspace = config.cli_unstable().doctest_in_workspace;

    let batches: CargoResult<Vec<_>> = compilation
        .to_doc_test
        .iter()
        .filter(|doctest_info| {
            if doctest_xcompile {
                true
            } else {
                match doctest_info.unit.kind {
                    CompileKind::Host => true,
                    CompileKind::Target(target) => {
                        target.short_name() == compilation.host // Skip doctests, -Zdoctest-xcompile not enabled.
                    }
                }
            }
        })
        .map(|doctest_info| {
            let Doctest {
                args,
                unstable_opts,
                unit,
                linker,
                script_meta,
            } = doctest_info;

            config.shell().status("Doc-tests", unit.target.name())?;
            let mut cmd = compilation.rustdoc_process(unit, *script_meta)?;
            cmd.arg("--crate-name").arg(&unit.target.crate_name());
            cmd.arg("--test");

            if !test_args.contains(&"--color=never") && config.shell().err_supports_color() {
                cmd.arg("--color=always");
            }

            if doctest_in_workspace {
                add_path_args(ws, unit, &mut cmd);
                // FIXME(swatinem): remove the `unstable-options` once rustdoc stabilizes the `test-run-directory` option
                cmd.arg("-Z").arg("unstable-options");
                cmd.arg("--test-run-directory")
                    .arg(unit.pkg.root().to_path_buf());
            } else {
                cmd.arg(unit.target.src_path().path().unwrap());
            }

            if doctest_xcompile {
                if let CompileKind::Target(target) = unit.kind {
                    // use `rustc_target()` to properly handle JSON target paths
                    cmd.arg("--target").arg(target.rustc_target());
                }
                cmd.arg("-Zunstable-options");
                cmd.arg("--enable-per-target-ignores");
                if let Some((runtool, runtool_args)) = compilation.target_runner(unit.kind) {
                    cmd.arg("--runtool").arg(runtool);
                    for arg in runtool_args {
                        cmd.arg("--runtool-arg").arg(arg);
                    }
                }
                if let Some(linker) = linker {
                    let mut joined = OsString::from("linker=");
                    joined.push(linker);
                    cmd.arg("-C").arg(joined);
                }
            }

            for &rust_dep in &[
                &compilation.deps_output[&unit.kind],
                &compilation.deps_output[&CompileKind::Host],
            ] {
                let mut arg = OsString::from("dependency=");
                arg.push(rust_dep);
                cmd.arg("-L").arg(arg);
            }

            for native_dep in compilation.native_dirs.iter() {
                cmd.arg("-L").arg(native_dep);
            }

            for arg in test_args {
                cmd.arg("--test-args").arg(arg);
            }

            if config.shell().verbosity() == Verbosity::Quiet {
                cmd.arg("--test-args").arg("--quiet");
            }

            cmd.args(args);

            if *unstable_opts {
                cmd.arg("-Zunstable-options");
            }

            let exe_display = cmd.to_string();

            let error_display = TestBatchErrorDisplay {
                target_kind: unit.target.kind().clone(),
                target_name: unit.target.name().to_string(),
                pkg_name: unit.pkg.name().to_string(),
            };

            Ok(TestBatch {
                display: TestBatchDisplay {
                    exe_display,
                    cmd_display: None,
                },
                error_display,
                cmd,
            })
        })
        .collect();

    let batches = batches?;
    let errors = run_test_batches(batches, options, config)?;

    let errors: Vec<_> = errors.into_iter().map(|(_, e)| e).collect();
    Ok((Test::Doc, errors))
}

struct TestOutput {
    pub exe_display: String,
    pub output: Option<OutOrErr>,
}

enum OutOrErr {
    Out(String),
    Err(String),
}

struct TestBatch {
    pub display: TestBatchDisplay,
    pub error_display: TestBatchErrorDisplay,
    pub cmd: ProcessBuilder,
}

struct TestBatchErrorDisplay {
    pub target_kind: TargetKind,
    pub target_name: String,
    pub pkg_name: String,
}

#[derive(Clone)]
struct TestBatchDisplay {
    pub exe_display: String,
    pub cmd_display: Option<String>,
}

fn run_test_batches(
    batches: Vec<TestBatch>,
    options: &TestOptions,
    config: &Config,
) -> CargoResult<Vec<(TestBatch, ProcessError)>> {
    let test_jobs = options.compile_opts.build_config.test_jobs;

    if test_jobs == 1 {
        let mut errors = vec![];
        for batch in batches {
            config
                .shell()
                .concise(|shell| shell.status("Running", &batch.display.exe_display))?;
            config
                .shell()
                .verbose(|shell| shell.status("Running", &batch.cmd))?;

            if let Err(e) = batch.cmd.exec() {
                let e = e.downcast::<ProcessError>().unwrap();
                errors.push((batch, e))
            };
        }

        Ok(errors)
    } else {
        // used for sending messages from the run test loop to the printer loop
        let (tx_print, rx_print) = std::sync::mpsc::channel::<TestOutput>();
        let exe_displays: Vec<_> = batches.iter().map(|x| x.display.to_owned()).collect();

        // runs the tests
        let fail_fast = !options.no_fail_fast;
        let run_tests_handle =
            std::thread::spawn(move || run_tests_loop(batches, &tx_print, fail_fast, test_jobs));

        // sorts and prints the test results as they come in
        printer_loop(exe_displays, rx_print, config);

        run_tests_handle.join().unwrap()
    }
}

fn get_exe_display(unit: &Unit, path: &PathBuf, cwd: &Path) -> String {
    if let TargetKind::Test = unit.target.kind() {
        let test_path = unit.target.src_path().path().unwrap();
        format!(
            "{} ({})",
            test_path
                .strip_prefix(unit.pkg.root())
                .unwrap_or(test_path)
                .display(),
            path.strip_prefix(cwd).unwrap_or(path).display()
        )
    } else {
        format!(
            "unittests ({})",
            path.strip_prefix(cwd).unwrap_or(path).display()
        )
    }
}

fn print_display(display: &TestBatchDisplay, config: &Config) {
    config
        .shell()
        .concise(|shell| shell.status("Running", &display.exe_display))
        .unwrap();
    config
        .shell()
        .verbose(|shell| {
            shell.status("Running", {
                match &display.cmd_display {
                    Some(x) => x,
                    None => "",
                }
            })
        })
        .unwrap();
}

fn print_line(line: &OutOrErr, config: &Config) {
    match line {
        OutOrErr::Out(line) => writeln!(config.shell().out(), "{}", line).unwrap(),
        OutOrErr::Err(line) => writeln!(config.shell().err(), "{}", line).unwrap(),
    }
}

// this function takes an unordered heap of messages from all running threads and presents
// them in chronological order, caching the rest
fn printer_loop(
    exe_displays: Vec<TestBatchDisplay>,
    rx_print: Receiver<TestOutput>,
    config: &Config,
) {
    if exe_displays.len() == 0 {
        return;
    }

    // used to cache messages that are not ready to be displayed
    let mut cache: HashMap<String, Vec<Option<OutOrErr>>> = exe_displays
        .iter()
        .map(|x| (x.exe_display.to_owned(), Vec::<Option<OutOrErr>>::new()))
        .collect();

    // used to keep track of what crate we are currently on
    let mut queue: VecDeque<TestBatchDisplay> = exe_displays.into_iter().map(|x| x).collect();

    let mut current_exe = queue.pop_front().unwrap();
    print_display(&current_exe, config);
    loop {
        // receive print messages in whatever order they arrive
        match rx_print.recv() {
            // somewhere in the middle of a stream of messages for an exe test crate
            Ok(TestOutput {
                exe_display,
                output: Some(output),
            }) => {
                if exe_display == current_exe.exe_display {
                    print_line(&output, config);
                } else {
                    cache.get_mut(&exe_display).unwrap().push(Some(output));
                }
            }
            // no more messages will arrive from this exe test crate
            // time to start printing messages from the next crate
            Ok(TestOutput {
                exe_display,
                output: None,
            }) => {
                if exe_display == current_exe.exe_display {
                    // loop through the cache and print as many messages as we can
                    // it could be that several crates have already finished
                    'mrloopy: loop {
                        match queue.pop_front() {
                            Some(new_exe) => {
                                current_exe = new_exe;
                                print_display(&current_exe, config);
                            }
                            None => return,
                        }

                        for val in cache.get(&current_exe.exe_display).unwrap() {
                            match val {
                                Some(line) => print_line(line, config),
                                None => {
                                    continue 'mrloopy;
                                }
                            }
                        }

                        break;
                    }
                } else {
                    cache.get_mut(&exe_display).unwrap().push(None);
                }
            }

            Err(_) => {
                // we should never normally get here
                break;
            }
        }
    }
}

fn run_tests_loop(
    batches: Vec<TestBatch>,
    tx_print: &Sender<TestOutput>,
    fail_fast: bool,
    test_jobs: u32,
) -> CargoResult<Vec<(TestBatch, ProcessError)>> {
    let (tx_done, rx_done) = std::sync::mpsc::channel::<Option<()>>();

    // every time a job is marked as done it frees up a spot for a new job to start
    // so marking test_jobs as done here effectively allows us to run test crates in parallel
    for _ in 0..test_jobs {
        tx_done.send(Some(())).unwrap();
    }

    let mut run_test_handles = vec![];
    for batch in batches {
        let tx_print_clone = tx_print.clone();
        let tx_done_clone = tx_done.clone();

        let handle = std::thread::spawn(
            move || -> Result<std::process::Output, (TestBatch, ProcessError)> {
                let mut has_error = false;
                let exe_display = batch.display.exe_display.to_string();
                let result = batch
                    .cmd
                    .exec_with_streaming(
                        &mut |line| {
                            if let Err(_) = tx_print_clone.send(TestOutput {
                                exe_display: batch.display.exe_display.to_owned(),
                                output: Some(OutOrErr::Out(line.to_string())),
                            }) {
                                println!("out-of-order: {}", line);
                            }
                            Ok(())
                        },
                        &mut |line| {
                            if let Err(_) = tx_print_clone.send(TestOutput {
                                exe_display: batch.display.exe_display.to_owned(),
                                output: Some(OutOrErr::Err(line.to_string())),
                            }) {
                                eprintln!("out-of-order: {}", line);
                            };
                            Ok(())
                        },
                        false,
                    )
                    .map_err(|e| {
                        has_error = true;
                        let e = e.downcast::<ProcessError>().unwrap();
                        (batch, e)
                    });

                tx_print_clone
                    .send(TestOutput {
                        exe_display,
                        output: None,
                    })
                    .unwrap();

                if has_error && fail_fast {
                    // fail fast
                    tx_done_clone.send(None).unwrap();
                } else {
                    // mark this job as done so we can pick up another job
                    tx_done_clone.send(Some(())).unwrap();
                }

                result
            },
        );

        run_test_handles.push(handle);

        // this blocks until jobs are marked as done
        if rx_done.recv().unwrap().is_none() {
            // fail fast
            break;
        }
    }

    let mut errors = Vec::new();

    // capture any errors
    for handle in run_test_handles {
        if let Err((batch, e)) = handle.join().unwrap() {
            errors.push((batch, e));
        }
    }

    Ok(errors)
}
