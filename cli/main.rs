// Copyright 2018-2025 the Deno authors. MIT license.

mod args;
mod cache;
mod cdp;
mod factory;
mod file_fetcher;
mod graph_container;
mod graph_util;
mod http_util;
mod jsr;
mod lsp;
mod module_loader;
mod node;
mod npm;
mod ops;
mod registry;
mod resolver;
mod standalone;
mod task_runner;
mod tools;
mod tsc;
mod type_checker;
mod util;
mod worker;

pub mod sys {
  #[allow(clippy::disallowed_types)] // ok, definition
  pub type CliSys = sys_traits::impls::RealSys;
}

use std::collections::HashMap;
use std::env;
use std::future::Future;
use std::io::IsTerminal;
use std::ops::Deref;
use std::path::PathBuf;
use std::sync::Arc;

use args::TaskFlags;
use deno_core::anyhow::Context;
use deno_core::error::AnyError;
use deno_core::futures::FutureExt;
use deno_core::unsync::JoinHandle;
use deno_lib::util::result::any_and_jserrorbox_downcast_ref;
use deno_lib::util::result::js_error_downcast_ref;
use deno_lib::worker::LibWorkerFactoryRoots;
use deno_resolver::npm::ByonmResolvePkgFolderFromDenoReqError;
use deno_resolver::npm::ResolvePkgFolderFromDenoReqError;
use deno_runtime::UnconfiguredRuntime;
use deno_runtime::WorkerExecutionMode;
use deno_runtime::fmt_errors::format_js_error;
use deno_runtime::tokio_util::create_and_run_current_thread_with_maybe_metrics;
use deno_telemetry::OtelConfig;
use deno_terminal::colors;
use factory::CliFactory;

const MODULE_NOT_FOUND: &str = "Module not found";
const UNSUPPORTED_SCHEME: &str = "Unsupported scheme";

use self::args::load_env_variables_from_env_file;
use self::util::draw_thread::DrawThread;
use crate::args::DenoSubcommand;
use crate::args::Flags;
use crate::args::flags_from_vec;
use crate::args::get_default_v8_flags;
use crate::util::display;
use crate::util::v8::get_v8_flags_from_env;
use crate::util::v8::init_v8_flags;

#[cfg(feature = "dhat-heap")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

/// Ensures that all subcommands return an i32 exit code and an [`AnyError`] error type.
trait SubcommandOutput {
  fn output(self) -> Result<i32, AnyError>;
}

impl SubcommandOutput for Result<i32, AnyError> {
  fn output(self) -> Result<i32, AnyError> {
    self
  }
}

impl SubcommandOutput for Result<(), AnyError> {
  fn output(self) -> Result<i32, AnyError> {
    self.map(|_| 0)
  }
}

impl SubcommandOutput for Result<(), std::io::Error> {
  fn output(self) -> Result<i32, AnyError> {
    self.map(|_| 0).map_err(|e| e.into())
  }
}

/// Ensure that the subcommand runs in a task, rather than being directly executed. Since some of these
/// futures are very large, this prevents the stack from getting blown out from passing them by value up
/// the callchain (especially in debug mode when Rust doesn't have a chance to elide copies!).
#[inline(always)]
fn spawn_subcommand<F: Future<Output = T> + 'static, T: SubcommandOutput>(
  f: F,
) -> JoinHandle<Result<i32, AnyError>> {
  // the boxed_local() is important in order to get windows to not blow the stack in debug
  deno_core::unsync::spawn(
    async move { f.map(|r| r.output()).await }.boxed_local(),
  )
}

async fn run_subcommand(
  flags: Arc<Flags>,
  unconfigured_runtime: Option<UnconfiguredRuntime>,
  roots: LibWorkerFactoryRoots,
) -> Result<i32, AnyError> {
  let handle = match flags.subcommand.clone() {
    DenoSubcommand::Add(add_flags) => spawn_subcommand(async {
      tools::pm::add(flags, add_flags, tools::pm::AddCommandName::Add).await
    }),
    DenoSubcommand::Remove(remove_flags) => {
      spawn_subcommand(async { tools::pm::remove(flags, remove_flags).await })
    }
    DenoSubcommand::Bench(bench_flags) => spawn_subcommand(async {
      if bench_flags.watch.is_some() {
        tools::bench::run_benchmarks_with_watch(flags, bench_flags)
          .boxed_local()
          .await
      } else {
        tools::bench::run_benchmarks(flags, bench_flags).await
      }
    }),
    DenoSubcommand::Bundle(bundle_flags) => spawn_subcommand(async {
      log::warn!(
        "⚠️ {} is experimental and subject to changes",
        colors::cyan("deno bundle")
      );
      tools::bundle::bundle(flags, bundle_flags).await
    }),
    DenoSubcommand::Deploy => spawn_subcommand(async {
      tools::deploy::deploy(Arc::unwrap_or_clone(flags)).await
    }),
    DenoSubcommand::Doc(doc_flags) => {
      spawn_subcommand(async { tools::doc::doc(flags, doc_flags).await })
    }
    DenoSubcommand::Eval(eval_flags) => spawn_subcommand(async {
      tools::run::eval_command(flags, eval_flags).await
    }),
    DenoSubcommand::Cache(cache_flags) => spawn_subcommand(async move {
      tools::installer::install_from_entrypoints(flags, &cache_flags.files)
        .await
    }),
    DenoSubcommand::Check(check_flags) => {
      spawn_subcommand(
        async move { tools::check::check(flags, check_flags).await },
      )
    }
    DenoSubcommand::Clean(clean_flags) => {
      spawn_subcommand(
        async move { tools::clean::clean(flags, clean_flags).await },
      )
    }
    DenoSubcommand::Compile(compile_flags) => spawn_subcommand(async {
      if compile_flags.eszip {
        tools::compile::compile_eszip(flags, compile_flags)
          .boxed_local()
          .await
      } else {
        tools::compile::compile(flags, compile_flags).await
      }
    }),
    DenoSubcommand::Coverage(coverage_flags) => spawn_subcommand(async move {
      let reporter =
        crate::tools::coverage::reporter::create(coverage_flags.r#type.clone());
      tools::coverage::cover_files(
        flags,
        coverage_flags.files.include,
        coverage_flags.files.ignore,
        coverage_flags.include,
        coverage_flags.exclude,
        coverage_flags.output,
        &[&*reporter],
      )
    }),
    DenoSubcommand::Fmt(fmt_flags) => {
      spawn_subcommand(
        async move { tools::fmt::format(flags, fmt_flags).await },
      )
    }
    DenoSubcommand::Init(init_flags) => {
      spawn_subcommand(async { tools::init::init_project(init_flags).await })
    }
    DenoSubcommand::Info(info_flags) => {
      spawn_subcommand(async { tools::info::info(flags, info_flags).await })
    }
    DenoSubcommand::Install(install_flags) => spawn_subcommand(async {
      tools::installer::install_command(flags, install_flags).await
    }),
    DenoSubcommand::JSONReference(json_reference) => {
      spawn_subcommand(async move {
        display::write_to_stdout_ignore_sigpipe(
          &deno_core::serde_json::to_vec_pretty(&json_reference.json).unwrap(),
        )
      })
    }
    DenoSubcommand::Jupyter(jupyter_flags) => spawn_subcommand(async {
      tools::jupyter::kernel(flags, jupyter_flags).await
    }),
    DenoSubcommand::Uninstall(uninstall_flags) => spawn_subcommand(async {
      tools::installer::uninstall(flags, uninstall_flags).await
    }),
    DenoSubcommand::Lsp => spawn_subcommand(async move {
      if std::io::stderr().is_terminal() {
        log::warn!(
          "{} command is intended to be run by text editors and IDEs and shouldn't be run manually.

  Visit https://docs.deno.com/runtime/getting_started/setup_your_environment/ for instruction
  how to setup your favorite text editor.

  Press Ctrl+C to exit.
        ", colors::cyan("deno lsp"));
      }
      lsp::start().await
    }),
    DenoSubcommand::Lint(lint_flags) => spawn_subcommand(async {
      if lint_flags.rules {
        tools::lint::print_rules_list(
          lint_flags.json,
          lint_flags.maybe_rules_tags,
        );
        Ok(())
      } else {
        tools::lint::lint(flags, lint_flags).await
      }
    }),
    DenoSubcommand::Outdated(update_flags) => {
      spawn_subcommand(
        async move { tools::pm::outdated(flags, update_flags).await },
      )
    }
    DenoSubcommand::Repl(repl_flags) => {
      spawn_subcommand(async move { tools::repl::run(flags, repl_flags).await })
    }
    DenoSubcommand::Run(run_flags) => spawn_subcommand(async move {
      if run_flags.is_stdin() {
        // these futures are boxed to prevent stack overflows on Windows
        tools::run::run_from_stdin(flags.clone(), unconfigured_runtime, roots)
          .boxed_local()
          .await
      } else if flags.eszip {
        tools::run::run_eszip(flags, run_flags, unconfigured_runtime, roots)
          .boxed_local()
          .await
      } else {
        let result = tools::run::run_script(
          WorkerExecutionMode::Run,
          flags.clone(),
          run_flags.watch,
          unconfigured_runtime,
          roots.clone(),
        )
        .await;
        match result {
          Ok(v) => Ok(v),
          Err(script_err) => {
            if let Some(
              worker::CreateCustomWorkerError::ResolvePkgFolderFromDenoReq(
                ResolvePkgFolderFromDenoReqError::Byonm(
                  ByonmResolvePkgFolderFromDenoReqError::UnmatchedReq(_),
                ),
              ),
            ) = any_and_jserrorbox_downcast_ref::<
              worker::CreateCustomWorkerError,
            >(&script_err)
            {
              if flags.node_modules_dir.is_none() {
                let mut flags = flags.deref().clone();
                let watch = match &flags.subcommand {
                  DenoSubcommand::Run(run_flags) => run_flags.watch.clone(),
                  _ => unreachable!(),
                };
                flags.node_modules_dir =
                  Some(deno_config::deno_json::NodeModulesDirMode::None);
                // use the current lockfile, but don't write it out
                if flags.frozen_lockfile.is_none() {
                  flags.internal.lockfile_skip_write = true;
                }
                return tools::run::run_script(
                  WorkerExecutionMode::Run,
                  Arc::new(flags),
                  watch,
                  None,
                  roots,
                )
                .boxed_local()
                .await;
              }
            }
            let script_err_msg = script_err.to_string();
            if script_err_msg.starts_with(MODULE_NOT_FOUND)
              || script_err_msg.starts_with(UNSUPPORTED_SCHEME)
            {
              if run_flags.bare {
                let mut cmd = args::clap_root();
                cmd.build();
                let command_names = cmd
                  .get_subcommands()
                  .map(|command| command.get_name())
                  .collect::<Vec<_>>();
                let suggestions =
                  args::did_you_mean(&run_flags.script, command_names);
                if !suggestions.is_empty() && !run_flags.script.contains('.') {
                  let mut error =
                    clap::error::Error::<clap::error::DefaultFormatter>::new(
                      clap::error::ErrorKind::InvalidSubcommand,
                    )
                    .with_cmd(&cmd);
                  error.insert(
                    clap::error::ContextKind::SuggestedSubcommand,
                    clap::error::ContextValue::Strings(suggestions),
                  );

                  Err(error.into())
                } else {
                  Err(script_err)
                }
              } else {
                let mut new_flags = flags.deref().clone();
                let task_flags = TaskFlags {
                  cwd: None,
                  task: Some(run_flags.script.clone()),
                  is_run: true,
                  recursive: false,
                  filter: None,
                  eval: false,
                };
                new_flags.subcommand = DenoSubcommand::Task(task_flags.clone());
                let result = tools::task::execute_script(
                  Arc::new(new_flags),
                  task_flags.clone(),
                )
                .await;
                match result {
                  Ok(v) => Ok(v),
                  Err(_) => {
                    // Return script error for backwards compatibility.
                    Err(script_err)
                  }
                }
              }
            } else {
              Err(script_err)
            }
          }
        }
      }
    }),
    DenoSubcommand::Serve(serve_flags) => spawn_subcommand(async move {
      tools::serve::serve(flags, serve_flags, unconfigured_runtime, roots).await
    }),
    DenoSubcommand::Task(task_flags) => spawn_subcommand(async {
      tools::task::execute_script(flags, task_flags).await
    }),
    DenoSubcommand::Test(test_flags) => {
      spawn_subcommand(async {
        if let Some(ref coverage_dir) = test_flags.coverage_dir {
          if !test_flags.coverage_raw_data_only || test_flags.clean {
            // Keeps coverage_dir contents only when --coverage-raw-data-only is set and --clean is not set
            let _ = std::fs::remove_dir_all(coverage_dir);
          }
          std::fs::create_dir_all(coverage_dir)
            .with_context(|| format!("Failed creating: {coverage_dir}"))?;
          // this is set in order to ensure spawned processes use the same
          // coverage directory

          #[allow(clippy::undocumented_unsafe_blocks)]
          unsafe {
            env::set_var(
              "DENO_COVERAGE_DIR",
              PathBuf::from(coverage_dir).canonicalize()?,
            )
          };
        }

        if test_flags.watch.is_some() {
          tools::test::run_tests_with_watch(flags, test_flags).await
        } else {
          tools::test::run_tests(flags, test_flags).await
        }
      })
    }
    DenoSubcommand::Completions(completions_flags) => {
      spawn_subcommand(async move {
        display::write_to_stdout_ignore_sigpipe(&completions_flags.buf)
      })
    }
    DenoSubcommand::Types => spawn_subcommand(async move {
      let types = tsc::get_types_declaration_file_text();
      display::write_to_stdout_ignore_sigpipe(types.as_bytes())
    }),
    #[cfg(feature = "upgrade")]
    DenoSubcommand::Upgrade(upgrade_flags) => spawn_subcommand(async {
      tools::upgrade::upgrade(flags, upgrade_flags).await
    }),
    #[cfg(not(feature = "upgrade"))]
    DenoSubcommand::Upgrade(_) => exit_with_message(
      "This deno was built without the \"upgrade\" feature. Please upgrade using the installation method originally used to install Deno.",
      1,
    ),
    DenoSubcommand::Vendor => exit_with_message(
      "⚠️ `deno vendor` was removed in Deno 2.\n\nSee the Deno 1.x to 2.x Migration Guide for migration instructions: https://docs.deno.com/runtime/manual/advanced/migrate_deprecations",
      1,
    ),
    DenoSubcommand::Publish(publish_flags) => spawn_subcommand(async {
      tools::publish::publish(flags, publish_flags).await
    }),
    DenoSubcommand::Help(help_flags) => spawn_subcommand(async move {
      use std::io::Write;

      let mut stream = anstream::AutoStream::new(
        std::io::stdout(),
        if colors::use_color() {
          anstream::ColorChoice::Auto
        } else {
          anstream::ColorChoice::Never
        },
      );

      match stream.write_all(help_flags.help.ansi().to_string().as_bytes()) {
        Ok(()) => Ok(()),
        Err(e) => match e.kind() {
          std::io::ErrorKind::BrokenPipe => Ok(()),
          _ => Err(e),
        },
      }
    }),
  };

  handle.await?
}

#[allow(clippy::print_stderr)]
fn setup_panic_hook() {
  // This function does two things inside of the panic hook:
  // - Tokio does not exit the process when a task panics, so we define a custom
  //   panic hook to implement this behaviour.
  // - We print a message to stderr to indicate that this is a bug in Deno, and
  //   should be reported to us.
  let orig_hook = std::panic::take_hook();
  std::panic::set_hook(Box::new(move |panic_info| {
    eprintln!("\n============================================================");
    eprintln!("Deno has panicked. This is a bug in Deno. Please report this");
    eprintln!("at https://github.com/denoland/deno/issues/new.");
    eprintln!("If you can reliably reproduce this panic, include the");
    eprintln!("reproduction steps and re-run with the RUST_BACKTRACE=1 env");
    eprintln!("var set and include the backtrace in your report.");
    eprintln!();
    eprintln!("Platform: {} {}", env::consts::OS, env::consts::ARCH);
    eprintln!("Version: {}", deno_lib::version::DENO_VERSION_INFO.deno);
    eprintln!("Args: {:?}", env::args().collect::<Vec<_>>());
    eprintln!();

    // Panic traces are not supported for custom/development builds.
    #[cfg(feature = "panic-trace")]
    {
      let info = &deno_lib::version::DENO_VERSION_INFO;
      let version =
        if info.release_channel == deno_lib::shared::ReleaseChannel::Canary {
          format!("{}+{}", deno_lib::version::DENO_VERSION, info.git_hash)
        } else {
          info.deno.to_string()
        };

      let trace = deno_panic::trace();
      eprintln!("View stack trace at:");
      eprintln!(
        "https://panic.deno.com/v{}/{}/{}",
        version,
        env!("TARGET"),
        trace
      );
    }

    orig_hook(panic_info);
    deno_runtime::exit(1);
  }));

  fn error_handler(file: &str, line: i32, message: &str) {
    // Override C++ abort with a rust panic, so we
    // get our message above and a nice backtrace.
    panic!("Fatal error in {file}:{line}: {message}");
  }

  deno_core::v8::V8::set_fatal_error_handler(error_handler);
}

fn exit_with_message(message: &str, code: i32) -> ! {
  log::error!(
    "{}: {}",
    colors::red_bold("error"),
    message.trim_start_matches("error: ")
  );
  deno_runtime::exit(code);
}

fn exit_for_error(error: AnyError) -> ! {
  let error_string = match js_error_downcast_ref(&error) {
    Some(e) => format_js_error(e),
    None => format!("{error:?}"),
  };

  exit_with_message(&error_string, 1);
}

pub(crate) fn unstable_exit_cb(feature: &str, api_name: &str) {
  log::error!(
    "Unstable API '{api_name}'. The `--unstable-{}` flag must be provided.",
    feature
  );
  deno_runtime::exit(70);
}

pub fn main() {
  #[cfg(feature = "dhat-heap")]
  let profiler = dhat::Profiler::new_heap();

  setup_panic_hook();

  init_logging(None, None);

  util::unix::raise_fd_limit();
  util::windows::ensure_stdio_open();
  #[cfg(windows)]
  {
    deno_subprocess_windows::disable_stdio_inheritance();
    colors::enable_ansi(); // For Windows 10
  }
  deno_runtime::deno_permissions::set_prompt_callbacks(
    Box::new(util::draw_thread::DrawThread::hide),
    Box::new(util::draw_thread::DrawThread::show),
  );

  rustls::crypto::aws_lc_rs::default_provider()
    .install_default()
    .unwrap();

  let args: Vec<_> = env::args_os().collect();
  let future = async move {
    let roots = LibWorkerFactoryRoots::default();

    #[cfg(unix)]
    let (waited_unconfigured_runtime, waited_args) =
      match wait_for_start(&args, roots.clone()) {
        Some(f) => match f.await {
          Ok(v) => match v {
            Some((u, a)) => (Some(u), Some(a)),
            None => (None, None),
          },
          Err(e) => {
            panic!("Failure from control sock: {e}");
          }
        },
        None => (None, None),
      };

    #[cfg(not(unix))]
    let (waited_unconfigured_runtime, waited_args) = (None, None);

    let args = waited_args.unwrap_or(args);

    // NOTE(lucacasonato): due to new PKU feature introduced in V8 11.6 we need to
    // initialize the V8 platform on a parent thread of all threads that will spawn
    // V8 isolates.
    let flags = resolve_flags_and_init(args).await?;

    if waited_unconfigured_runtime.is_none() {
      init_v8(&flags);
    }

    run_subcommand(Arc::new(flags), waited_unconfigured_runtime, roots).await
  };

  let result = create_and_run_current_thread_with_maybe_metrics(future);

  #[cfg(feature = "dhat-heap")]
  drop(profiler);

  match result {
    Ok(exit_code) => deno_runtime::exit(exit_code),
    Err(err) => exit_for_error(err),
  }
}

async fn resolve_flags_and_init(
  args: Vec<std::ffi::OsString>,
) -> Result<Flags, AnyError> {
  let mut flags = match flags_from_vec(args) {
    Ok(flags) => flags,
    Err(err @ clap::Error { .. })
      if err.kind() == clap::error::ErrorKind::DisplayVersion =>
    {
      // Ignore results to avoid BrokenPipe errors.
      let _ = err.print();
      deno_runtime::exit(0);
    }
    Err(err) => exit_for_error(AnyError::from(err)),
  };

  load_env_variables_from_env_file(flags.env_file.as_ref(), flags.log_level);
  flags.unstable_config.fill_with_env();
  if std::env::var("DENO_COMPAT").is_ok() {
    flags.unstable_config.enable_node_compat();
  }
  if flags.node_conditions.is_empty() {
    if let Ok(conditions) = std::env::var("DENO_CONDITIONS") {
      flags.node_conditions = conditions
        .split(",")
        .map(|c| c.trim().to_string())
        .collect();
    }
  }

  // Tunnel is initialized before OTEL since
  // OTEL data is submitted via the tunnel.
  if let Some(host) = flags
    .connected
    .clone()
    .or_else(|| env::var("DENO_CONNECTED").ok())
  {
    if let Err(err) = initialize_tunnel(&host, &flags).await {
      exit_for_error(AnyError::from(err))
    }
  }

  let otel_config = flags.otel_config();
  init_logging(flags.log_level, Some(otel_config.clone()));
  deno_telemetry::init(
    deno_lib::version::otel_runtime_config(),
    otel_config.clone(),
  )?;

  // TODO(bartlomieju): remove in Deno v2.5 and hard error then.
  if flags.unstable_config.legacy_flag_enabled {
    log::warn!(
      "⚠️  {}",
      colors::yellow(
        "The `--unstable` flag has been removed in Deno 2.0. Use granular `--unstable-*` flags instead.\nLearn more at: https://docs.deno.com/runtime/manual/tools/unstable_flags"
      )
    );
  }

  Ok(flags)
}

fn init_v8(flags: &Flags) {
  let default_v8_flags = match flags.subcommand {
    DenoSubcommand::Lsp => vec![
      "--stack-size=1024".to_string(),
      "--js-explicit-resource-management".to_string(),
      // Using same default as VSCode:
      // https://github.com/microsoft/vscode/blob/48d4ba271686e8072fc6674137415bc80d936bc7/extensions/typescript-language-features/src/configuration/configuration.ts#L213-L214
      "--max-old-space-size=3072".to_string(),
    ],
    _ => get_default_v8_flags(),
  };

  let env_v8_flags = get_v8_flags_from_env();
  let is_single_threaded = env_v8_flags
    .iter()
    .chain(&flags.v8_flags)
    .any(|flag| flag == "--single-threaded");
  init_v8_flags(&default_v8_flags, &flags.v8_flags, env_v8_flags);
  let v8_platform = if is_single_threaded {
    Some(::deno_core::v8::Platform::new_single_threaded(true).make_shared())
  } else {
    None
  };

  // TODO(bartlomieju): remove last argument once Deploy no longer needs it
  deno_core::JsRuntime::init_platform(
    v8_platform,
    /* import assertions enabled */ false,
  );
}

fn init_logging(
  maybe_level: Option<log::Level>,
  otel_config: Option<OtelConfig>,
) {
  deno_lib::util::logger::init(deno_lib::util::logger::InitLoggingOptions {
    maybe_level,
    otel_config,
    // it was considered to hold the draw thread's internal lock
    // across logging, but if outputting to stderr blocks then that
    // could potentially block other threads that access the draw
    // thread's state
    on_log_start: DrawThread::hide,
    on_log_end: DrawThread::show,
  })
}

#[cfg(unix)]
#[allow(clippy::type_complexity)]
fn wait_for_start(
  args: &[std::ffi::OsString],
  roots: LibWorkerFactoryRoots,
) -> Option<
  impl Future<
    Output = Result<
      Option<(UnconfiguredRuntime, Vec<std::ffi::OsString>)>,
      AnyError,
    >,
  > + use<>,
> {
  let startup_snapshot = deno_snapshots::CLI_SNAPSHOT?;
  let addr = std::env::var("DENO_UNSTABLE_CONTROL_SOCK").ok()?;

  #[allow(clippy::undocumented_unsafe_blocks)]
  unsafe {
    std::env::remove_var("DENO_UNSTABLE_CONTROL_SOCK")
  };

  let argv0 = args[0].clone();

  Some(async move {
    use tokio::io::AsyncBufReadExt;
    use tokio::io::AsyncRead;
    use tokio::io::AsyncWrite;
    use tokio::io::AsyncWriteExt;
    use tokio::io::BufReader;
    use tokio::net::TcpListener;
    use tokio::net::UnixSocket;
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    use tokio_vsock::VsockAddr;
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    use tokio_vsock::VsockListener;

    init_v8(&Flags::default());

    let unconfigured = deno_runtime::UnconfiguredRuntime::new::<
      deno_resolver::npm::DenoInNpmPackageChecker,
      crate::npm::CliNpmResolver,
      crate::sys::CliSys,
    >(deno_runtime::UnconfiguredRuntimeOptions {
      startup_snapshot,
      create_params: deno_lib::worker::create_isolate_create_params(
        &crate::sys::CliSys::default(),
      ),
      shared_array_buffer_store: Some(roots.shared_array_buffer_store.clone()),
      compiled_wasm_module_store: Some(
        roots.compiled_wasm_module_store.clone(),
      ),
      additional_extensions: vec![],
      enable_raw_imports: false,
    });

    let (rx, mut tx): (
      Box<dyn AsyncRead + Unpin>,
      Box<dyn AsyncWrite + Send + Unpin>,
    ) = match addr.split_once(':') {
      Some(("tcp", addr)) => {
        let listener = TcpListener::bind(addr).await?;
        let (stream, _) = listener.accept().await?;
        let (rx, tx) = stream.into_split();
        (Box::new(rx), Box::new(tx))
      }
      Some(("unix", path)) => {
        let socket = UnixSocket::new_stream()?;
        socket.bind(path)?;
        let listener = socket.listen(1)?;
        let (stream, _) = listener.accept().await?;
        let (rx, tx) = stream.into_split();
        (Box::new(rx), Box::new(tx))
      }
      #[cfg(any(target_os = "linux", target_os = "macos"))]
      Some(("vsock", addr)) => {
        let Some((cid, port)) = addr.split_once(':') else {
          deno_core::anyhow::bail!("invalid vsock addr");
        };
        let cid = if cid == "-1" { u32::MAX } else { cid.parse()? };
        let port = port.parse()?;
        let addr = VsockAddr::new(cid, port);
        let listener = VsockListener::bind(addr)?;
        let (stream, _) = listener.accept().await?;
        let (rx, tx) = stream.into_split();
        (Box::new(rx), Box::new(tx))
      }
      _ => {
        deno_core::anyhow::bail!("invalid control sock");
      }
    };

    let mut buf = Vec::with_capacity(1024);
    BufReader::new(rx).read_until(b'\n', &mut buf).await?;

    tokio::spawn(async move {
      deno_runtime::deno_http::SERVE_NOTIFIER.notified().await;

      #[derive(deno_core::serde::Serialize)]
      enum Event {
        Serving,
      }

      let mut buf = deno_core::serde_json::to_vec(&Event::Serving).unwrap();
      buf.push(b'\n');
      let _ = tx.write_all(&buf).await;
    });

    #[derive(deno_core::serde::Deserialize)]
    struct Start {
      cwd: String,
      args: Vec<String>,
      env: Vec<(String, String)>,
    }

    let cmd: Start = deno_core::serde_json::from_slice(&buf)?;

    std::env::set_current_dir(cmd.cwd)?;

    for (k, v) in cmd.env {
      // SAFETY: We're doing this before any threads are created.
      unsafe { std::env::set_var(k, v) };
    }

    let args = [argv0]
      .into_iter()
      .chain(cmd.args.into_iter().map(Into::into))
      .collect();

    Ok(Some((unconfigured, args)))
  })
}

async fn auth_tunnel() -> Result<String, deno_core::anyhow::Error> {
  let mut child = tokio::process::Command::new(env::current_exe()?)
    .args(["deploy", "tunnel-login"])
    .spawn()?;
  let out = child.wait().await?;

  if !out.success() {
    deno_runtime::exit(1);
  }

  Ok(tools::deploy::get_token_entry()?.get_password()?)
}

#[allow(clippy::print_stderr)]
async fn initialize_tunnel(
  host: &str,
  flags: &Flags,
) -> Result<(), deno_core::anyhow::Error> {
  let mut factory = CliFactory::from_flags(Arc::new(flags.clone()));
  let mut cli_options = factory.cli_options()?;
  let deploy_config = cli_options.start_dir.to_deploy_config()?;
  if deploy_config.is_none() {
    let _ = tools::deploy::get_token_entry()?.delete_credential();
  }

  let token = if let Ok(token) = std::env::var("DENO_UNSTABLE_TUNNEL_TOKEN") {
    token
  } else {
    match tools::deploy::get_token_entry()?.get_password() {
      Ok(token) => token,
      Err(keyring::Error::NoEntry) => {
        let token = auth_tunnel().await?;

        if deploy_config.is_none() {
          // we regenerate the factory & CliOptions since auth_tunnel updates
          // the config file with the deploy config, only if it was not set previously.
          factory = CliFactory::from_flags(Arc::new(flags.clone()));
          cli_options = factory.cli_options()?;
        }

        token
      }
      Err(e) => {
        exit_for_error(e.into());
      }
    }
  };

  let (org, app) = if let (Ok(org), Ok(app)) = (
    std::env::var("DENO_UNSTABLE_TUNNEL_ORG"),
    std::env::var("DENO_UNSTABLE_TUNNEL_APP"),
  ) {
    (org, app)
  } else {
    let deploy_config = cli_options
      .start_dir
      .to_deploy_config()?
      .expect("auth to be called");

    (deploy_config.org, deploy_config.app)
  };

  let Some(addr) = tokio::net::lookup_host(&host).await?.next() else {
    return Ok(());
  };
  let Some((hostname, _)) = host.split_once(':') else {
    return Ok(());
  };

  let cert_store_provider = factory.root_cert_store_provider();
  let root_cert_store = cert_store_provider.get_or_try_init()?.clone();

  let tls_config = deno_runtime::deno_tls::create_client_config(
    Some(root_cert_store),
    vec![],
    None,
    deno_runtime::deno_tls::TlsKeys::Null,
    deno_runtime::deno_tls::SocketUse::GeneralSsl,
  )?;

  let mut metadata = HashMap::new();
  metadata.insert(
    "hostname".into(),
    deno_runtime::deno_os::sys_info::hostname(),
  );
  if let Some(entrypoint) = match &flags.subcommand {
    DenoSubcommand::Run(run_flags) => Some(run_flags.script.to_owned()),
    DenoSubcommand::Serve(serve_flags) => Some(serve_flags.script.to_owned()),
    DenoSubcommand::Repl(_) => Some("<repl>".into()),
    DenoSubcommand::Eval(_) => Some("<eval>".into()),
    _ => None,
  } {
    metadata.insert("entrypoint".into(), entrypoint);
  }

  let (tunnel, mut events) =
    match deno_runtime::deno_net::tunnel::TunnelConnection::connect(
      addr,
      hostname.to_owned(),
      tls_config.clone(),
      deno_runtime::deno_net::tunnel::Authentication::App {
        token,
        org: org.clone(),
        app: app.clone(),
      },
      metadata.clone(),
    )
    .await
    {
      Ok(res) => res,
      Err(deno_runtime::deno_net::tunnel::Error::Unauthorized) => {
        tools::deploy::get_token_entry()?.delete_credential()?;

        let token = auth_tunnel().await?;
        deno_runtime::deno_net::tunnel::TunnelConnection::connect(
          addr,
          hostname.to_owned(),
          tls_config,
          deno_runtime::deno_net::tunnel::Authentication::App {
            token,
            org,
            app,
          },
          metadata.clone(),
        )
        .await?
      }
      Err(e) => return Err(e.into()),
    };

  let addr = tunnel.local_addr()?;

  let endpoint = if addr.port() == 443 {
    format!("https://{}", addr.hostname())
  } else {
    format!("https://{}:{}", addr.hostname(), addr.port())
  };

  tokio::spawn(async move {
    while let Some(event) = events.next().await {
      use deno_runtime::deno_net::tunnel::Event;
      match event {
        Event::Routed => {
          log::info!(
            "{}",
            colors::green(format!("You are connected to {endpoint}!"))
          );
        }
        Event::Reconnect(d) => {
          log::info!(
            "{}",
            colors::green(format!(
              "Reconnecting tunnel in {}s...",
              d.as_secs()
            ))
          );
        }
        _ => {}
      }
    }
  });

  if let Some(metadata) = tunnel.metadata() {
    for (k, v) in metadata.env {
      // SAFETY: We're doing this before any threads are created.
      unsafe {
        std::env::set_var(k, v);
      }
    }
  }

  deno_runtime::deno_net::tunnel::set_tunnel(tunnel);

  Ok(())
}
