//! Grok's `<root>/.grok/config.toml`, edited with `toml_edit` so that its
//! comments and layout survive. Each server is a `[mcp_servers.<name>]` table.

use super::codex::{alone, open};
use super::{Field, Fields, FileRefusal, Rendered, Vendor, VendorFile, VendorRefusal};
use super::{Kind, args, command, env, headers, refuse, url};
use crate::catalog::{EnvValue, HeaderValue, Server, Transport, VendorName};
use std::path::Path;
use toml_edit::{Array, InlineTable, Item, Table, Value};

pub struct Grok;

impl Vendor for Grok {
    fn name(&self) -> VendorName {
        VendorName::Grok
    }

    fn title(&self) -> &'static str {
        "Grok"
    }

    fn target(&self) -> &'static str {
        ".grok/config.toml"
    }

    fn render(&self, name: &str, server: &Server) -> Result<Rendered, VendorRefusal> {
        let refuse = |reason: String| refuse(VendorName::Grok, name, reason);
        let mut fields = Fields::default();
        match server.transport {
            Transport::Stdio => {
                fields.text("command", command(server));
                fields.list("args", args(server));
                let (mut literals, mut forwarded) = (Vec::new(), Vec::new());
                for (key, value) in env(server) {
                    match value {
                        EnvValue::Literal(v) => literals.push((key.clone(), v.clone())),
                        EnvValue::Secret { .. } => {
                            let var = value.secret_var(key).expect("a secret names a variable");
                            if var != key {
                                return Err(refuse(format!(
                                    "`env.{key}` reads the variable `{var}`, and Grok forwards \
                                     a secret variable under its own name"
                                )));
                            }
                            forwarded.push(var.to_string());
                        }
                    }
                }
                fields.map("env", literals);
                fields.list("env_vars", forwarded);
            }
            Transport::Sse => {
                return Err(refuse(
                    "Grok connects to streamable HTTP servers only, not SSE".into(),
                ));
            }
            Transport::Http => {
                fields.text("url", url(server));
                let (mut literals, mut from_env, mut bearer) = (Vec::new(), Vec::new(), None);
                for (header, value) in headers(server) {
                    match value {
                        HeaderValue::Literal(v) => literals.push((header.clone(), v.clone())),
                        HeaderValue::Secret { env, scheme: None } => {
                            from_env.push((header.clone(), env.clone()));
                        }
                        HeaderValue::Secret {
                            env,
                            scheme: Some(scheme),
                        } if header.eq_ignore_ascii_case("authorization")
                            && scheme.eq_ignore_ascii_case("bearer") =>
                        {
                            bearer = Some(env);
                        }
                        HeaderValue::Secret {
                            scheme: Some(scheme),
                            ..
                        } => {
                            return Err(refuse(format!(
                                "the secret header `{header}` has the scheme `{scheme}`, and \
                                 Grok sends a secret header either whole or as \
                                 `Authorization: Bearer`"
                            )));
                        }
                    }
                }
                if let Some(env) = bearer {
                    fields.text("bearer_token_env_var", env);
                }
                fields.map("http_headers", literals);
                fields.map("env_http_headers", from_env);
            }
        }
        let mut table = Table::new();
        for (key, field) in fields.0 {
            let value = match field {
                Field::Text(s) => Value::from(s),
                Field::List(values) => Value::Array(values.into_iter().collect::<Array>()),
                Field::Map(values) => {
                    Value::InlineTable(values.into_iter().collect::<InlineTable>())
                }
            };
            table.insert(key, Item::Value(value));
        }
        Ok(Rendered {
            text: alone(name, &Item::Table(table)),
        })
    }

    fn open(&self, path: &Path, bytes: Option<&[u8]>) -> Result<VendorFile, FileRefusal> {
        let kind = Kind::Toml(open(path, bytes)?);
        Ok(VendorFile { kind })
    }
}

#[cfg(test)]
mod tests {
    use super::Grok;
    use crate::vendor::fixtures::*;

    #[test]
    fn a_stdio_server_forwards_a_secret_by_name_and_writes_literals_only() {
        assert_eq!(
            file_with(&Grok, "files", STDIO),
            r#"[mcp_servers.files]
command = "npx"
args = ["-y", "@example/files-mcp@1.4.2"]
env = { LOG_LEVEL = "debug" }
env_vars = ["FILES_TOKEN"]
"#
        );
    }

    #[test]
    fn a_renamed_secret_is_refused_for_grok() {
        assert_eq!(
            refusal(&Grok, RENAMED),
            "`env.GITHUB_TOKEN` reads the variable `GH_PAT`, and Grok forwards a secret variable \
             under its own name"
        );
    }

    #[test]
    fn an_sse_remote_is_refused_for_grok() {
        assert_eq!(
            refusal(&Grok, SSE),
            "Grok connects to streamable HTTP servers only, not SSE"
        );
    }
}
