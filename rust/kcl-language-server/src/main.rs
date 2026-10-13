//! The `kcl` lsp server.

#![deny(missing_docs)]

use anyhow::Result;
use anyhow::bail;
use clap::Parser;
use slog::Drain;
use tower_lsp::LspService;
use tower_lsp::Server as LspServer;
use tracing_subscriber::Layer;
use tracing_subscriber::prelude::*;

lazy_static::lazy_static! {
/// Initialize the logger.
    // We need a slog::Logger for steno and when we export out the logs from re-exec-ed processes.
    pub static ref LOGGER: slog::Logger = {
        let decorator = slog_term::TermDecorator::new().build();
        let drain = slog_term::FullFormat::new(decorator).build().fuse();
        let drain = slog_async::Async::new(drain).build().fuse();
        slog::Logger::root(drain, slog::o!())
    };
}

/// This doc string acts as a help message when the user runs '--help'
/// as do all doc strings on fields.
#[derive(Parser, Debug, Clone)]
#[clap(version = clap::crate_version!(), author = clap::crate_authors!("\n"))]
pub struct Opts {
    /// Print debug info
    #[clap(short, long)]
    pub debug: bool,

    /// Print logs as json
    #[clap(short, long)]
    pub json: bool,

    /// The subcommand to run.
    #[clap(subcommand)]
    pub subcmd: SubCommand,
}

impl Opts {
    /// Setup our logger.
    pub fn create_logger(&self) -> slog::Logger {
        if self.json {
            let drain = slog_json::Json::default(std::io::stderr()).fuse();
            self.async_root_logger(drain)
        } else {
            let decorator = slog_term::TermDecorator::new().build();
            let drain = slog_term::FullFormat::new(decorator).build().fuse();
            self.async_root_logger(drain)
        }
    }

    fn async_root_logger<T>(&self, drain: T) -> slog::Logger
    where
        T: slog::Drain + Send + 'static,
        <T as slog::Drain>::Err: std::fmt::Debug,
    {
        let level = if self.debug {
            slog::Level::Debug
        } else {
            slog::Level::Info
        };

        let level_drain = slog::LevelFilter(drain, level).fuse();
        let async_drain = slog_async::Async::new(level_drain).build().fuse();
        slog::Logger::root(async_drain, slog::o!())
    }
}

/// A subcommand for our cli.
#[derive(Parser, Debug, Clone)]
pub enum SubCommand {
    /// Run the server.
    Server(kcl_language_server::KclLspServerSubCommand),
}

#[tokio::main]
async fn main() -> Result<()> {
    let opts: Opts = Opts::parse();

    let level_filter = if opts.debug {
        tracing_subscriber::filter::LevelFilter::DEBUG
    } else {
        tracing_subscriber::filter::LevelFilter::INFO
    };

    // Format fields using the provided closure.
    // We want to make this very concise otherwise the logs are not able to be read by humans.
    let format = tracing_subscriber::fmt::format::debug_fn(|writer, field, value| {
        if format!("{field}") == "message" {
            write!(writer, "{field}: {value:?}")
        } else {
            write!(writer, "{field}")
        }
    })
    // Separate each field with a comma.
    // This method is provided by an extension trait in the
    // `tracing-subscriber` prelude.
    .delimited(", ");

    let (json, plain) = if opts.json {
        // Cloud run likes json formatted logs if possible.
        // See: https://cloud.google.com/run/docs/logging
        // We could probably format these specifically for cloud run if we wanted,
        // will save that as a TODO: https://cloud.google.com/run/docs/logging#special-fields
        (
            Some(tracing_subscriber::fmt::layer().json().with_filter(level_filter)),
            None,
        )
    } else {
        (
            None,
            Some(
                tracing_subscriber::fmt::layer()
                    .pretty()
                    .fmt_fields(format)
                    .with_filter(level_filter),
            ),
        )
    };

    // Initialize the tracing.
    tracing_subscriber::registry().with(json).with(plain).init();

    if let Err(err) = run_cmd(&opts).await {
        bail!("running cmd `{:?}` failed: {:?}", opts.subcmd, err);
    }

    Ok(())
}

async fn run_cmd(opts: &Opts) -> Result<()> {
    match &opts.subcmd {
        SubCommand::Server(s) => {
            let (service, socket) = standalone_service().await;

            // TODO find a way to ctrl+c on windows.
            #[cfg(not(target_os = "windows"))]
            {
                // For Cloud run & ctrl+c, shutdown gracefully.
                // "The main process inside the container will receive SIGTERM, and after a grace period,
                // SIGKILL."
                // Registering SIGKILL here will panic at runtime, so let's avoid that.
                use signal_hook::consts::SIGINT;
                use signal_hook::consts::SIGTERM;
                use signal_hook::iterator::Signals;
                let mut signals = Signals::new([SIGINT, SIGTERM])?;

                tokio::spawn(async move {
                    if let Some(sig) = signals.forever().next() {
                        log::info!("received signal: {sig:?}");
                        log::info!("triggering cleanup...");

                        // Exit the process.
                        log::info!("all clean, exiting!");
                        std::process::exit(0);
                    }
                });
            }

            if s.stdio {
                // Listen on stdin and stdout.
                let stdin = tokio::io::stdin();
                let stdout = tokio::io::stdout();
                LspServer::new(stdin, stdout, socket).serve(service).await;
            } else {
                // Listen on a tcp stream.
                let listener = tokio::net::TcpListener::bind(&format!("0.0.0.0:{}", s.socket)).await?;
                let (stream, _) = listener.accept().await?;
                let (read, write) = tokio::io::split(stream);
                LspServer::new(read, write, socket).serve(service).await;
            }
        }
    }

    Ok(())
}

// The standalone editor has no live engine session. Mock execution still
// checks argument types and collects deprecations without authentication.
async fn standalone_service() -> (LspService<kcl_language_server::KclLspBackend>, tower_lsp::ClientSocket) {
    let executor = kcl_lib::ExecutorContext::new_mock(None).await;
    LspService::new(|client| {
        kcl_language_server::KclLspBackend::new(client, Some(executor), kittycad::Client::new("")).unwrap()
    })
}

#[cfg(test)]
mod tests {
    use tower_lsp::LanguageServer;
    use tower_lsp::lsp_types::*;

    use super::standalone_service;

    #[tokio::test]
    async fn standalone_execution_diagnostics() {
        // Exercise the executable's factory, rather than a mock-only test
        // factory that could pass while the executable disables execution.
        let (service, socket) = standalone_service().await;
        // This test checks the backend map directly. Close the unused socket
        // so outgoing notifications cannot fill its undrained queue.
        drop(socket);
        let server = service.inner();
        server.initialize(InitializeParams::default()).await.unwrap();
        server.initialized(InitializedParams {}).await;
        let uri: Url = "file:///test.kcl".parse().unwrap();
        server
            .did_open(DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri.clone(),
                    language_id: "kcl".into(),
                    version: 1,
                    text: "@settings(kclVersion = 2.0)\np = startSketchOn(XY)\n".into(),
                },
            })
            .await;
        {
            let diagnostics = server.diagnostics_map.get(uri.as_str()).expect("missing deprecation");
            let warning = diagnostics
                .iter()
                .find(|d| d.message.contains("`startSketchOn` is deprecated"))
                .expect("missing startSketchOn deprecation");
            assert_eq!(warning.severity, Some(DiagnosticSeverity::WARNING));
            assert_eq!(warning.range.start.line, 1);
        }
        server.did_change(DidChangeTextDocumentParams {
            text_document: VersionedTextDocumentIdentifier { uri: uri.clone(), version: 2 },
            content_changes: vec![TextDocumentContentChangeEvent {
                range: None, range_length: None,
                text: "@settings(kclVersion = 2.0)\np = sketch(on = XY) {\n  bottom = line(start = [0, 0], end = [4, 0])\n  right = line(start = bottom.end, end = [4, 3])\n}\n".into(),
            }],
        }).await;
        {
            let diagnostics = server.diagnostics_map.get(uri.as_str()).expect("missing type error");
            let error = diagnostics
                .iter()
                .find(|d| d.message.contains("Point2d") && d.message.contains("Segment"))
                .expect("missing Point2d/Segment mismatch");
            assert_eq!(error.severity, Some(DiagnosticSeverity::ERROR));
            assert_eq!(error.range.start.line, 3);
            assert!(!diagnostics.iter().any(|d| d.message.contains("startSketchOn")));
        }
        server.did_change(DidChangeTextDocumentParams {
            text_document: VersionedTextDocumentIdentifier { uri: uri.clone(), version: 3 },
            content_changes: vec![TextDocumentContentChangeEvent {
                range: None, range_length: None,
                text: "@settings(kclVersion = 2.0)\np = sketch(on = XY) {\n  bottom = line(start = [0, 0], end = [4, 0])\n  right = line(start = [4, 0], end = [4, 3])\n}\n".into(),
            }],
        }).await;
        assert!(server.diagnostics_map.get(uri.as_str()).is_none());
    }
}
