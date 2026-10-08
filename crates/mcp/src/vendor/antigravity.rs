//! Antigravity's `<root>/.agents/mcp_config.json`. Antigravity expands no
//! `${NAME}`, in `env` or in headers, but a stdio server inherits its
//! environment: so a secret variable is left out of the entry, and a server
//! with a secret header is refused for Antigravity alone (MCP spec §4.2).
//! Antigravity has no SSE; its remote key is `serverUrl`.

use super::{Fields, FileRefusal, Rendered, Vendor, VendorFile, VendorRefusal};
use super::{Kind, args, command, env, headers, json, refuse, url};
use crate::catalog::{EnvValue, HeaderValue, Server, Transport, VendorName};
use std::path::Path;

pub struct Antigravity;

impl Vendor for Antigravity {
    fn name(&self) -> VendorName {
        VendorName::Antigravity
    }

    fn title(&self) -> &'static str {
        "Antigravity"
    }

    fn target(&self) -> &'static str {
        ".agents/mcp_config.json"
    }

    fn render(&self, name: &str, server: &Server) -> Result<Rendered, VendorRefusal> {
        let refuse = |reason: String| refuse(VendorName::Antigravity, name, reason);
        let mut fields = Fields::default();
        match server.transport {
            Transport::Stdio => {
                fields.text("command", command(server));
                fields.list("args", args(server));
                let mut literals = Vec::new();
                for (key, value) in env(server) {
                    match value {
                        EnvValue::Literal(v) => literals.push((key.clone(), v.clone())),
                        EnvValue::Secret { .. } => {
                            let var = value.secret_var(key).expect("a secret names a variable");
                            if var != key {
                                return Err(refuse(format!(
                                    "`env.{key}` reads the variable `{var}`, but a stdio server \
                                     inherits Antigravity's environment, so a secret cannot be \
                                     read under another name"
                                )));
                            }
                        }
                    }
                }
                fields.map("env", literals);
            }
            Transport::Sse => {
                return Err(refuse(
                    "Antigravity does not support the legacy SSE transport; only a streamable \
                     HTTP endpoint or a stdio server"
                        .into(),
                ));
            }
            Transport::Http => {
                fields.text("serverUrl", url(server));
                let mut literals = Vec::new();
                for (header, value) in headers(server) {
                    match value {
                        HeaderValue::Literal(v) => literals.push((header.clone(), v.clone())),
                        HeaderValue::Secret { .. } => {
                            return Err(refuse(format!(
                                "the header `{header}` is a secret, and Antigravity expands no \
                                 `${{…}}` in a header, so a secret header cannot reach the server"
                            )));
                        }
                    }
                }
                fields.map("headers", literals);
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
    use super::Antigravity;
    use crate::vendor::fixtures::*;

    #[test]
    fn a_stdio_server_leaves_a_secret_out_for_the_server_to_inherit() {
        assert_eq!(
            file_with(&Antigravity, "files", STDIO),
            r#"{
  "mcpServers": {
    "files": {
      "command": "npx",
      "args": [
        "-y",
        "@example/files-mcp@1.4.2"
      ],
      "env": {
        "LOG_LEVEL": "debug"
      }
    }
  }
}
"#
        );
    }

    #[test]
    fn a_server_whose_env_is_all_secret_gets_no_env() {
        assert_eq!(
            file_with(&Antigravity, "files", ONLY_SECRET),
            r#"{
  "mcpServers": {
    "files": {
      "command": "files-mcp"
    }
  }
}
"#
        );
    }

    #[test]
    fn a_remote_is_written_under_server_url_with_its_literal_headers() {
        assert_eq!(
            file_with(&Antigravity, "docs", PLAIN_REMOTE),
            r#"{
  "mcpServers": {
    "docs": {
      "serverUrl": "https://mcp.example.com/mcp",
      "headers": {
        "X-Client": "fl"
      }
    }
  }
}
"#
        );
    }

    #[test]
    fn an_sse_remote_is_refused_for_antigravity() {
        assert_eq!(
            refusal(&Antigravity, SSE),
            "Antigravity does not support the legacy SSE transport; only a streamable HTTP \
             endpoint or a stdio server"
        );
    }

    #[test]
    fn any_secret_header_is_refused_for_antigravity() {
        for (body, header) in [(BEARER, "Authorization"), (WHOLE, "X-Api-Key")] {
            assert_eq!(
                refusal(&Antigravity, body),
                format!(
                    "the header `{header}` is a secret, and Antigravity expands no `${{…}}` in a \
                     header, so a secret header cannot reach the server"
                )
            );
        }
    }

    #[test]
    fn a_secret_under_another_name_is_refused_for_antigravity() {
        assert_eq!(
            refusal(&Antigravity, RENAMED),
            "`env.GITHUB_TOKEN` reads the variable `GH_PAT`, but a stdio server inherits \
             Antigravity's environment, so a secret cannot be read under another name"
        );
    }
}
