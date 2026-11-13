use clap::Subcommand;
use fl_core::agent::{
    RoleError, RoleRecord, RoleReviewPolicy, delete_role, get_role, list_roles, upsert_role,
};
use fl_core::state::StateScope;

use crate::cmd::state::StateEnv;

#[derive(Subcommand)]
pub enum RoleCmd {
    /// List roles (builtins in registry order, then customs by slug).
    List {
        #[arg(long)]
        json: bool,
    },
    /// Show one role by slug (`lab_manager` aliases `lab-manager`).
    Show {
        slug: String,
        #[arg(long)]
        json: bool,
    },
    /// Create or update a role (custom role, or an override of a builtin).
    Upsert {
        slug: String,
        #[arg(long)]
        label: String,
        #[arg(long)]
        description: Option<String>,
        /// One of `lead-review`, `lab-manager-review`, `peer-review`
        /// (snake_case aliases accepted).
        #[arg(long)]
        review_policy: Option<String>,
        #[arg(long)]
        prompt_template: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Delete a custom role (builtin roles are refused).
    Delete { slug: String },
}

/// One human-readable line per role: slug, label, policy, builtin/custom.
pub fn role_line(record: &RoleRecord) -> String {
    format!(
        "{} {} {} {}",
        record.slug,
        record.label,
        record.review_policy,
        if record.builtin { "builtin" } else { "custom" }
    )
}

fn print_role(record: &RoleRecord, json: bool) -> Result<(), String> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(record).map_err(|e| e.to_string())?
        );
    } else {
        println!("{}", role_line(record));
        println!("  description: {}", record.description);
        println!("  prompt_template: {}", record.prompt_template);
    }
    Ok(())
}

pub fn run(env: &StateEnv<'_>, cmd: RoleCmd) -> Result<(), String> {
    let store = env.open_for_scope(Some(&StateScope::Workspace))?;
    match cmd {
        RoleCmd::List { json } => {
            let roles = list_roles(&store).map_err(|e| e.to_string())?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&roles).map_err(|e| e.to_string())?
                );
            } else {
                for role in &roles {
                    println!("{}", role_line(role));
                }
            }
            Ok(())
        }
        RoleCmd::Show { slug, json } => {
            let role = get_role(&store, &slug)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| {
                    format!("no such role `{slug}`: run `fl role list` to see available roles")
                })?;
            print_role(&role, json)
        }
        RoleCmd::Upsert {
            slug,
            label,
            description,
            review_policy,
            prompt_template,
            json,
        } => {
            let existing = get_role(&store, &slug).map_err(|e| e.to_string())?;
            let policy = match review_policy {
                Some(raw) => raw
                    .parse::<RoleReviewPolicy>()
                    .map_err(|e: RoleError| e.to_string())?,
                None => existing
                    .as_ref()
                    .map(|r| r.review_policy)
                    .unwrap_or(RoleReviewPolicy::LeadReview),
            };
            let record = RoleRecord {
                slug: slug.clone(),
                label,
                description: description
                    .or_else(|| existing.as_ref().map(|r| r.description.clone()))
                    .unwrap_or_default(),
                builtin: false,
                review_policy: policy,
                prompt_template: prompt_template
                    .or_else(|| existing.as_ref().map(|r| r.prompt_template.clone()))
                    .unwrap_or_default(),
            };
            let saved = upsert_role(&store, record).map_err(|e| e.to_string())?;
            print_role(&saved, json)
        }
        RoleCmd::Delete { slug } => {
            delete_role(&store, &slug).map_err(|e| e.to_string())?;
            println!("deleted role {slug}");
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_core::agent::builtin_role_records;

    #[test]
    fn role_line_names_slug_label_policy_and_kind() {
        let roles = builtin_role_records();
        assert_eq!(
            role_line(roles.iter().find(|role| role.slug == "lead").unwrap()),
            "lead Project Lead lab-manager-review builtin"
        );
        let custom =
            RoleRecord::new("reviewer", "Reviewer", "", RoleReviewPolicy::PeerReview, "").unwrap();
        assert_eq!(role_line(&custom), "reviewer Reviewer peer-review custom");
    }
}
