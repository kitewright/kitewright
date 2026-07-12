//! kite-pdf entrypoint: a subcommand dispatcher.
//!
//! - `kite-pdf serve` (or no arguments) runs the HTTP render service.
//! - `kite-pdf render ...` renders a single document to a PDF file.

use anyhow::{bail, Context, Result};

use kite_pdf::request::{Margin, RenderRequest};
use kite_pdf::{http, AppState};

const USAGE: &str = "\
kite-pdf — HTML/Typst → PDF service + CLI

USAGE:
  kite-pdf [serve]                 Run the HTTP render service (default).
                                   Bind address: $KITE_PDF_BIND (default 0.0.0.0:8091).
  kite-pdf render [OPTIONS] -o OUT.pdf

RENDER OPTIONS:
  Chromium backend (HTML → PDF):
    --html-file <f>        Render this HTML file.
    --url <u>              Render this URL.
    --header-file <f>      Header template HTML (implies --display-header-footer).
    --footer-file <f>      Footer template HTML (implies --display-header-footer).
    --display-header-footer
    --print-background
    --margin-top/-bottom/-left/-right <css>   e.g. 20px, 1cm, 0.5in
  Typst backend (template + data → PDF):
    --template <f.typ>     Typst source file.
    --data <d.json>        JSON exposed to the template as sys.inputs.data.
  Shared:
    --engine <chromium|typst>   Force a backend (else inferred).
    --format <A4|Letter|Legal|A3>
    --landscape
    --scale <n>
  -o, --output <f>         Output PDF path (required for render).
  -h, --help               Show this help.
";

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("-h") | Some("--help") => {
            print!("{USAGE}");
            Ok(())
        }
        Some("render") => run_render(&args[1..]).await,
        Some("serve") | None => http::serve(AppState::new()).await,
        Some(other) => {
            eprintln!("unknown subcommand {other:?}\n");
            print!("{USAGE}");
            bail!("unknown subcommand {other:?}");
        }
    }
}

async fn run_render(args: &[String]) -> Result<()> {
    let mut req = RenderRequest::default();
    let mut margin = Margin::default();
    let mut output: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        // Pull the value that follows a flag, advancing the cursor.
        macro_rules! val {
            ($name:expr) => {{
                i += 1;
                args.get(i)
                    .cloned()
                    .with_context(|| format!("missing value for {}", $name))?
            }};
        }
        match arg {
            "--html-file" => req.html = Some(read_file(&val!("--html-file"))?),
            "--url" => req.url = Some(val!("--url")),
            "--template" => req.template = Some(read_file(&val!("--template"))?),
            "--data" => {
                let text = read_file(&val!("--data"))?;
                req.data =
                    Some(serde_json::from_str(&text).context("--data file is not valid JSON")?);
            }
            "--header-file" => {
                req.header_template = Some(read_file(&val!("--header-file"))?);
                req.display_header_footer = true;
            }
            "--footer-file" => {
                req.footer_template = Some(read_file(&val!("--footer-file"))?);
                req.display_header_footer = true;
            }
            "--display-header-footer" => req.display_header_footer = true,
            "--print-background" => req.print_background = true,
            "--landscape" => req.landscape = true,
            "--engine" => req.engine = Some(val!("--engine")),
            "--format" => req.format = Some(val!("--format")),
            "--scale" => {
                req.scale = Some(
                    val!("--scale")
                        .parse()
                        .context("--scale must be a number")?,
                )
            }
            "--margin-top" => margin.top = Some(val!("--margin-top")),
            "--margin-bottom" => margin.bottom = Some(val!("--margin-bottom")),
            "--margin-left" => margin.left = Some(val!("--margin-left")),
            "--margin-right" => margin.right = Some(val!("--margin-right")),
            "-o" | "--output" => output = Some(val!("--output")),
            "-h" | "--help" => {
                print!("{USAGE}");
                return Ok(());
            }
            other => bail!("unknown render option {other:?} (try --help)"),
        }
        i += 1;
    }

    if margin.top.is_some()
        || margin.bottom.is_some()
        || margin.left.is_some()
        || margin.right.is_some()
    {
        req.margin = Some(margin);
    }

    let output = output.context("missing -o/--output (path to the PDF to write)")?;

    let state = AppState::new();
    let bytes = kite_pdf::render(&state, &req)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    std::fs::write(&output, &bytes).with_context(|| format!("failed to write {output}"))?;
    // Best-effort clean shutdown so the browser child (chromium backend) is
    // never orphaned by the CLI.
    #[cfg(feature = "chromium")]
    state.engine.shutdown().await;
    eprintln!("wrote {} ({} bytes)", output, bytes.len());
    Ok(())
}

fn read_file(path: &str) -> Result<String> {
    std::fs::read_to_string(path).with_context(|| format!("failed to read {path}"))
}
