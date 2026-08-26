mod azure;
mod bitbucket;
mod chromium;
mod forgejo;
mod gitea;
mod gitee;
mod github;
mod github_device_flow;
mod gitlab;
mod sourcehut;
mod tangled;

pub use azure::*;
pub use bitbucket::*;
pub use chromium::*;
pub use forgejo::*;
pub use gitea::*;
pub use gitee::*;
pub use github::*;
pub use github_device_flow::*;
pub use gitlab::*;
pub use sourcehut::*;
pub use tangled::*;

#[cfg(test)]
mod tests {
    use git::{GitHostAuthKind, GitHostingProvider};
    use pretty_assertions::assert_eq;
    use url::Url;

    use super::*;

    fn url(raw: &str) -> Url {
        Url::parse(raw).unwrap()
    }

    // `auth_kind` decides which hosts the PR panel offers to connect. Returning
    // `Some` for a host whose API these providers cannot actually speak would
    // advertise support that fails partway through a review, so each case below
    // is a deliberate answer rather than an incidental one.

    #[test]
    fn test_github_is_connectable_on_public_and_enterprise_hosts() {
        assert_eq!(
            Github::public_instance().auth_kind(),
            Some(GitHostAuthKind::GitHub)
        );
        assert_eq!(
            Github::new("GitHub Enterprise", url("https://github.acme.com")).auth_kind(),
            Some(GitHostAuthKind::GitHub)
        );
    }

    #[test]
    fn test_gitlab_is_connectable_on_public_and_self_managed_hosts() {
        assert_eq!(
            Gitlab::public_instance().auth_kind(),
            Some(GitHostAuthKind::GitLab)
        );
        assert_eq!(
            Gitlab::new("GitLab Self-Managed", url("https://gitlab.acme.com")).auth_kind(),
            Some(GitHostAuthKind::GitLab)
        );
    }

    #[test]
    fn test_bitbucket_is_connectable_only_on_bitbucket_cloud() {
        assert_eq!(
            Bitbucket::public_instance().auth_kind(),
            Some(GitHostAuthKind::Bitbucket)
        );
    }

    // Data Center exposes `/rest/api/1.0`, which the Bitbucket calls do not
    // speak, so a self-hosted instance must not be offered at all.
    #[test]
    fn test_bitbucket_data_center_is_not_connectable() {
        assert_eq!(
            Bitbucket::new("Bitbucket Data Center", url("https://bitbucket.acme.com")).auth_kind(),
            None
        );
    }

    #[test]
    fn test_providers_without_pull_request_support_are_not_connectable() {
        assert_eq!(Gitea::public_instance().auth_kind(), None);
        assert_eq!(Forgejo::public_instance().auth_kind(), None);
        assert_eq!(SourceHut::public_instance().auth_kind(), None);
        assert_eq!(Tangled::public_instance().auth_kind(), None);
        assert_eq!(Gitee.auth_kind(), None);
        assert_eq!(Azure.auth_kind(), None);
        assert_eq!(Chromium.auth_kind(), None);
    }
}
