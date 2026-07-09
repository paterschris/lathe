use gpui::Action;
use ui::ContextMenu;

const GITHUB_HOST: &str = "github.com";
const BITBUCKET_HOST: &str = "bitbucket.org";

pub fn append_git_integrations(
    menu: ContextMenu,
    github_connected: Option<String>,
    bitbucket_connected: Option<String>,
) -> ContextMenu {
    let menu = append_host_action(menu, "GitHub", "GitHub", GITHUB_HOST, github_connected);
    append_host_action(
        menu,
        "Bitbucket",
        "Bitbucket Cloud",
        BITBUCKET_HOST,
        bitbucket_connected,
    )
}

fn append_host_action(
    menu: ContextMenu,
    disconnect_label: &'static str,
    connect_label: &'static str,
    host: &'static str,
    connected_login: Option<String>,
) -> ContextMenu {
    match connected_login {
        Some(login) => menu.action(
            format!("Disconnect {disconnect_label} ({login})"),
            zed_actions::DisconnectGitHost {
                host: host.to_string(),
            }
            .boxed_clone(),
        ),
        None => menu.action(
            format!("Connect {connect_label}…"),
            zed_actions::ConnectGitHost {
                host: host.to_string(),
            }
            .boxed_clone(),
        ),
    }
}
