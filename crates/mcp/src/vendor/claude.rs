//! Claude Code's `<root>/.mcp.json`. Claude Code expands `${NAME}` in `env`
//! and in headers, so a secret is written as a reference to its variable. A
//! remote entry always carries `type`: Claude Code skips a `url` without one.

use super::{Fields, FileRefusal, Rendered, Vendor, VendorFile, VendorRefusal};
use super::{Kind, args, command, env, headers, json, url};
use crate::catalog::{EnvValue, HeaderValue, Server, Transport, VendorName};
use std::path::Path;

pub struct Claude;

impl Vendor for Claude {
    fn name(&self) -> VendorName {
        VendorName::Claude
    }

    fn title(&self) -> &'static str {
        "Claude Code"
    }

    fn target(&self) -> &'static str {
        ".mcp.json"
    }

    fn render(&self, _name: &str, server: &Server) -> Result<Rendered, VendorRefusal> {
        let mut fields = Fields::default();
        match server.transport {
            Transport::Stdio => {
                fields.text("command", command(server));
                fields.list("args", args(server));
                let env = env(server).map(|(key, value)| {
                    let value = match value {
                        EnvValue::Literal(v) => v.clone(),
                        EnvValue::Secret { .. } => {
                            let var = value.secret_var(key).expect("a secret names a variable");
                            format!("${{{var}}}")
                        }
                    };
                    (key.clone(), value)
                });
                fields.map("env", env.collect());
            }
            Transport::Http | Transport::Sse => {
                fields.text("type", server.transport.as_str());
                fields.text("url", url(server));
                let headers = headers(server).map(|(name, value)| {
                    let value = match value {
                        HeaderValue::Literal(v) => v.clone(),
                        HeaderValue::Secret { env, scheme: None } => format!("${{{env}}}"),
                        HeaderValue::Secret {
                            env,
                            scheme: Some(scheme),
                        } => format!("{scheme} ${{{env}}}"),
                    };
                    (name.clone(), value)
                });
                fields.map("headers", headers.collect());
            }
        }
        Ok(json::render(&fields.0))
    }

    fn open(&self, path: &Path, bytes: Option<&[u8]>) -> Result<VendorFile, FileRefusal> {
        let kind = Kind::Json(json::open(path, bytes)?);
        Ok(VendorFile { kind })
    }
}

#[cfg(test)]
mod tests {
    use super::Claude;
    use crate::vendor::fixtures::*;

    #[test]
    fn a_stdio_server_gets_its_command_args_and_env_with_a_secret_as_a_reference() {
        assert_eq!(
            file_with(&Claude, "files", STDIO),
            r#"{
  "mcpServers": {
    "files": {
      "command": "npx",
      "args": [
        "-y",
        "@example/files-mcp@1.4.2"
      ],
      "env": {
        "FILES_TOKEN": "${FILES_TOKEN}",
        "LOG_LEVEL": "debug"
      }
    }
  }
}
"#
        );
    }

    #[test]
    fn a_secret_under_another_name_reads_that_variable() {
        assert_eq!(
            file_with(&Claude, "gh", RENAMED),
            r#"{
  "mcpServers": {
    "gh": {
      "command": "files-mcp",
      "env": {
        "GITHUB_TOKEN": "${GH_PAT}"
      }
    }
  }
}
"#
        );
    }

    #[test]
    fn an_http_remote_carries_its_type_and_a_secret_bearer_header_as_a_reference() {
        assert_eq!(
            file_with(&Claude, "docs", BEARER),
            r#"{
  "mcpServers": {
    "docs": {
      "type": "http",
      "url": "https://mcp.example.com/mcp",
      "headers": {
        "Authorization": "Bearer ${DOCS_TOKEN}",
        "X-Client": "fl"
      }
    }
  }
}
"#
        );
    }

    #[test]
    fn a_secret_header_with_no_scheme_is_the_reference_alone() {
        assert_eq!(
            file_with(&Claude, "docs", WHOLE),
            r#"{
  "mcpServers": {
    "docs": {
      "type": "http",
      "url": "https://mcp.example.com/mcp",
      "headers": {
        "X-Api-Key": "${DOCS_KEY}"
      }
    }
  }
}
"#
        );
    }

    #[test]
    fn an_sse_remote_is_written_with_its_type() {
        assert_eq!(
            file_with(&Claude, "events", SSE),
            r#"{
  "mcpServers": {
    "events": {
      "type": "sse",
      "url": "https://events.example.com/sse"
    }
  }
}
"#
        );
    }
}
