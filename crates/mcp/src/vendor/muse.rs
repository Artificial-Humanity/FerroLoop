//! Meta Muse's `<root>/.muse/settings.json`. Muse reads MCP servers from the
//! `"mcp_servers"` object in `.muse/settings.json` (with `"schema_version": 1`
//! on newly created files) and expands `${NAME}` in `env` and in headers.

use super::{Fields, FileRefusal, Rendered, Vendor, VendorFile, VendorRefusal};
use super::{Kind, args, command, env, headers, json, url};
use crate::catalog::{EnvValue, HeaderValue, Server, Transport, VendorName};
use std::path::Path;

const SERVERS: &str = "mcp_servers";
const DEFAULT_TOP: &[(&str, &str)] = &[("schema_version", "1")];

pub struct Muse;

impl Vendor for Muse {
    fn name(&self) -> VendorName {
        VendorName::Muse
    }

    fn title(&self) -> &'static str {
        "Muse"
    }

    fn target(&self) -> &'static str {
        ".muse/settings.json"
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
        let kind = Kind::Json(json::open_with(path, bytes, SERVERS, DEFAULT_TOP)?);
        Ok(VendorFile { kind })
    }
}

#[cfg(test)]
mod tests {
    use super::Muse;
    use crate::vendor::fixtures::*;

    #[test]
    fn a_stdio_server_writes_schema_version_and_mcp_servers_with_secret_reference() {
        assert_eq!(
            file_with(&Muse, "files", STDIO),
            r#"{
  "schema_version": 1,
  "mcp_servers": {
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
            file_with(&Muse, "gh", RENAMED),
            r#"{
  "schema_version": 1,
  "mcp_servers": {
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
            file_with(&Muse, "docs", BEARER),
            r#"{
  "schema_version": 1,
  "mcp_servers": {
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
}
