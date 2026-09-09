use std::path::PathBuf;
use std::process::Command;

use anyhow::Result;
use scriptmcp::catalog::Catalog;
use scriptmcp::config::Opts;
use scriptmcp::deno::DenoRuntime;
use serde_json::json;
use tempfile::TempDir;

fn deno_bin() -> Option<PathBuf> {
    let output = Command::new("deno").arg("--version").output().ok()?;
    output.status.success().then(|| PathBuf::from("deno"))
}

fn opts(scripts: PathBuf, deno: PathBuf) -> Opts {
    Opts {
        scripts,
        deno,
        allow_all: true,
        deno_args: Vec::new(),
        timeout: 30,
        bind: "127.0.0.1:8788".into(),
    }
}

#[tokio::test]
async fn introspects_and_invokes_a_script() -> Result<()> {
    let Some(deno) = deno_bin() else {
        eprintln!("skipping: deno is not on PATH");
        return Ok(());
    };

    let dir = TempDir::new()?;
    std::fs::write(
        dir.path().join("hello.ts"),
        r#"
export const name = "hello";
export const description = "Greet";
export const inputSchema = {
  type: "object",
  properties: { name: { type: "string" } },
  required: ["name"],
};
export default function hello({ name }: { name: string }) {
  return `Hello, ${name}!`;
}
"#,
    )?;

    let opts = opts(dir.path().to_path_buf(), deno);
    let runtime = DenoRuntime::new(&opts).await?;
    let catalog = Catalog::load(&opts, &runtime).await?;
    let tool = catalog.get("hello").expect("hello tool");
    assert_eq!(tool.meta.description, "Greet");

    let result = runtime
        .invoke(&tool.path, &[], &json!({ "name": "Ada" }))
        .await?;
    assert_eq!(result, json!("Hello, Ada!"));
    Ok(())
}
