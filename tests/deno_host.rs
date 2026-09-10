use std::path::PathBuf;
use std::process::Command;

use anyhow::Result;
use scriptmcp::catalog::Catalog;
use scriptmcp::config::Opts;
use scriptmcp::deno::DenoRuntime;
use scriptmcp::permissions::ScriptPermissions;
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
export default {
  name: "hello",
  description: "Greet",
  inputSchema: {
    type: "object",
    properties: { name: { type: "string" } },
    required: ["name"],
  },
  async run(args) {
    return `Hello, ${args.name}!`;
  },
};
"#,
    )?;

    let opts = opts(dir.path().to_path_buf(), deno);
    let runtime = DenoRuntime::new(&opts).await?;
    let catalog = Catalog::load(&opts.scripts, &runtime).await?;
    let tool = catalog.get("hello").expect("hello tool");
    assert_eq!(tool.meta.description, "Greet");

    let result = runtime
        .invoke(
            &tool.path,
            &ScriptPermissions::default(),
            &json!({ "name": "Ada" }),
        )
        .await?;
    assert_eq!(result, json!("Hello, Ada!"));
    Ok(())
}

#[tokio::test]
async fn rejects_args_that_fail_input_schema() -> Result<()> {
    let Some(deno) = deno_bin() else {
        eprintln!("skipping: deno is not on PATH");
        return Ok(());
    };

    let dir = TempDir::new()?;
    std::fs::write(
        dir.path().join("hello.ts"),
        r#"
export default {
  name: "hello",
  description: "Greet",
  inputSchema: {
    type: "object",
    properties: { name: { type: "string" } },
    required: ["name"],
  },
  async run(args) {
    return `Hello, ${args.name}!`;
  },
};
"#,
    )?;

    let opts = opts(dir.path().to_path_buf(), deno);
    let runtime = DenoRuntime::new(&opts).await?;
    let err = runtime
        .invoke(
            &dir.path().join("hello.ts"),
            &ScriptPermissions::default(),
            &json!({}),
        )
        .await
        .expect_err("missing required arg should fail");
    assert!(
        err.to_string().contains("missing required property"),
        "unexpected error: {err}"
    );
    Ok(())
}

#[tokio::test]
async fn surfaces_thrown_errors_as_invoke_failures() -> Result<()> {
    let Some(deno) = deno_bin() else {
        eprintln!("skipping: deno is not on PATH");
        return Ok(());
    };

    let dir = TempDir::new()?;
    std::fs::write(
        dir.path().join("boom.ts"),
        r#"
export default {
  name: "boom",
  description: "Throw",
  async run() {
    throw new TypeError("kaboom");
  },
};
"#,
    )?;
    std::fs::write(
        dir.path().join("reject.ts"),
        r#"
export default {
  name: "reject",
  description: "Reject",
  async run() {
    throw new Error("nope", { cause: new Error("root") });
  },
};
"#,
    )?;
    std::fs::write(
        dir.path().join("string_throw.ts"),
        r#"
export default {
  name: "string_throw",
  description: "Throw a string",
  async run() {
    throw "plain string";
  },
};
"#,
    )?;

    let opts = opts(dir.path().to_path_buf(), deno);
    let runtime = DenoRuntime::new(&opts).await?;
    let none = ScriptPermissions::default();

    let err = runtime
        .invoke(&dir.path().join("boom.ts"), &none, &json!({}))
        .await
        .expect_err("thrown TypeError should fail invoke");
    let message = err.to_string();
    assert!(
        message.contains("TypeError") && message.contains("kaboom"),
        "unexpected error: {message}"
    );
    assert!(
        !message.contains("    at "),
        "stack should not be the MCP error text: {message}"
    );

    let err = runtime
        .invoke(&dir.path().join("reject.ts"), &none, &json!({}))
        .await
        .expect_err("rejected promise should fail invoke");
    let message = err.to_string();
    assert!(
        message.contains("nope") && message.contains("root"),
        "unexpected error: {message}"
    );

    let err = runtime
        .invoke(&dir.path().join("string_throw.ts"), &none, &json!({}))
        .await
        .expect_err("thrown string should fail invoke");
    assert!(
        err.to_string().contains("plain string"),
        "unexpected error: {err}"
    );

    Ok(())
}
