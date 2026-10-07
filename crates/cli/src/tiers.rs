//! A routed store's two tiers as the CLI holds them (routing spec §1.3,
//! §2.6): the GitHub tier is opened on the first call that needs it, so
//! work on local items needs no network and no credential.

use crate::config::TrackerBinding;
use fl_core::TieredTracker;
use fl_core::ids::{Kind, ProjectId};
use fl_core::iri::Iri;
use fl_core::routing::{GithubTier, RoutingFault, Tier};
use fl_core::store::{StoreError, Tracker};
use fl_github::GithubTracker;
use std::cell::OnceCell;

/// How the GitHub tracker is opened: `open_github` in `main.rs`.
pub type Opener<'a> = Box<dyn Fn(&TrackerBinding) -> anyhow::Result<GithubTracker> + 'a>;

pub struct LazyGithub<'a> {
    binding: Option<TrackerBinding>,
    /// Where a binding goes, for the refusal when there is none.
    config: String,
    open: Opener<'a>,
    opened: OnceCell<GithubTracker>,
}

impl<'a> LazyGithub<'a> {
    pub fn new(binding: Option<TrackerBinding>, config: String, open: Opener<'a>) -> Self {
        Self {
            binding,
            config,
            open,
            opened: OnceCell::new(),
        }
    }

    /// The GitHub tracker, opened now if it is not yet (routing spec §2.6).
    /// An error that is not a store error keeps its whole message chain.
    pub fn open(&self) -> Result<&GithubTracker, StoreError> {
        if let Some(g) = self.opened.get() {
            return Ok(g);
        }
        let Some(b) = &self.binding else {
            return Err(RoutingFault::TierUnavailable {
                tier: Tier::Github,
                why: format!(
                    "this machine binds no GitHub repository for the project: add `tracker = \
                     {{ github = \"owner/repo\", credential = \"env\" }}` to its entry in {}",
                    self.config
                ),
            }
            .into());
        };
        let g = (self.open)(b).map_err(|e| match e.downcast::<StoreError>() {
            Ok(store) => store,
            Err(other) => StoreError::Backend(format!("{other:#}")),
        })?;
        Ok(self.opened.get_or_init(|| g))
    }
}

impl GithubTier for LazyGithub<'_> {
    fn available(&self) -> bool {
        self.binding.is_some()
    }

    /// An issue URL under the configured name — or, once GitHub is open,
    /// under the repository's name now, which differs after a rename and is
    /// the name the tracker writes. With no binding, none: an issue URL is
    /// then looked for in the local tier, where it may be an alias, and
    /// refused as the missing tier if it is not one (`issue_form`).
    fn claims(&self, id: &Iri) -> bool {
        let (Some((name, _)), Some(b)) = (fl_github::meta::parse_issue_url(id), &self.binding)
        else {
            return false;
        };
        name.eq_ignore_ascii_case(&b.github)
            || self
                .opened
                .get()
                .is_some_and(|g| name.eq_ignore_ascii_case(&g.repo().full_name))
    }

    fn issue_form(&self, id: &Iri) -> bool {
        fl_github::meta::is_issue_url(id)
    }

    fn tracker(&self) -> Result<&dyn Tracker, StoreError> {
        Ok(self.open()?)
    }

    fn require_private(&self) -> Result<(), StoreError> {
        self.open()?.require_private()
    }

    fn items_in_area(
        &self,
        project: &ProjectId,
        area: &str,
    ) -> Result<Vec<(Kind, Iri)>, StoreError> {
        self.open()?.items_in_area(project, area)
    }
}

/// A routed store's tiers, as a command sees them.
pub struct Tiers<'a> {
    pub router: &'a TieredTracker<'a>,
    pub github: &'a LazyGithub<'a>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Credential;
    use fl_github::fake::FakeGithub;
    use std::cell::Cell;

    fn binding() -> TrackerBinding {
        TrackerBinding {
            github: "acme/widgets".into(),
            credential: Credential::Env,
            ledger: None,
        }
    }

    fn issue(n: u64) -> Iri {
        Iri::parse(&format!("https://github.com/acme/widgets/issues/{n}")).unwrap()
    }

    // Routing spec §2.6: nothing is opened until a GitHub item is needed,
    // and the form of an id is read without opening anything.
    #[test]
    fn nothing_is_opened_until_a_github_item_is_needed() {
        let opened = Cell::new(0);
        let lazy = LazyGithub::new(
            Some(binding()),
            "the config".into(),
            Box::new(|_: &TrackerBinding| {
                opened.set(opened.get() + 1);
                anyhow::bail!("the credential is missing")
            }),
        );
        assert!(lazy.available());
        assert!(lazy.claims(&issue(4)));
        assert!(!lazy.claims(&Iri::parse("https://github.com/acme/other/issues/4").unwrap()));
        assert!(!lazy.claims(&fl_core::ids::seq_iri(4)));
        assert!(lazy.issue_form(&Iri::parse("https://github.com/acme/other/issues/4").unwrap()));
        assert!(!lazy.issue_form(&fl_core::ids::seq_iri(4)));
        assert_eq!(opened.get(), 0);
        let err = lazy.tracker().err().unwrap();
        assert!(
            matches!(err, StoreError::Backend(ref m) if m.contains("the credential is missing")),
            "{err:?}"
        );
        assert_eq!(opened.get(), 1);
    }

    // Routing spec §1.3: with no binding the tier is not available, and the
    // refusal names the config entry. It claims no id — the router looks in
    // the local tier first, where an issue URL may be an alias — but knows
    // an issue URL's form, to refuse one the local tier does not hold.
    #[test]
    fn with_no_binding_the_tier_is_unavailable_naming_the_config_entry() {
        let lazy = LazyGithub::new(
            None,
            "/c/fl/config.toml".into(),
            Box::new(|_: &TrackerBinding| unreachable!("no binding, nothing to open")),
        );
        assert!(!lazy.available());
        assert!(!lazy.claims(&issue(4)) && lazy.issue_form(&issue(4)));
        let msg = lazy.open().err().unwrap().to_string();
        assert!(
            msg.contains("binds no GitHub repository for the project")
                && msg.contains("tracker = { github")
                && msg.contains("/c/fl/config.toml"),
            "{msg}"
        );
    }

    #[test]
    fn an_open_that_fails_with_a_store_error_keeps_it_and_one_that_succeeds_is_kept() {
        let lazy = LazyGithub::new(
            Some(binding()),
            "c".into(),
            Box::new(
                |_: &TrackerBinding| Err(StoreError::RateLimited { reset: "r".into() }.into()),
            ),
        );
        assert!(matches!(lazy.open(), Err(StoreError::RateLimited { .. })));
        let fake = FakeGithub::start("acme/widgets");
        let opened = Cell::new(0);
        let lazy = LazyGithub::new(
            Some(binding()),
            "c".into(),
            Box::new(|b: &TrackerBinding| {
                opened.set(opened.get() + 1);
                let client = fl_github::Client::new(
                    &fake.url(),
                    Box::new(fl_github::EnvToken::from_lookup(|_| Some("t".into())).unwrap()),
                );
                Ok(GithubTracker::open(client, &b.github, &fl_core::MemStore::default())?.0)
            }),
        );
        lazy.tracker().unwrap();
        lazy.tracker().unwrap();
        assert_eq!(opened.get(), 1, "opened once");
    }
}
