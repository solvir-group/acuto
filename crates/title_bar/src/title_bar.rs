mod application_menu;
pub mod collab;
mod onboarding_banner;
mod plan_chip;
mod title_bar_settings;
mod update_version;

use crate::application_menu::{ApplicationMenu, show_menus};
use crate::plan_chip::PlanChip;
use agent_settings::{AgentSettings, WindowLayout};
use arrayvec::ArrayVec;
use git_ui_core::worktree_picker::WorktreePicker;
pub use platform_title_bar::{
    self, DraggedWindowTab, MergeAllWindows, MoveTabToNewWindow, PlatformTitleBar,
    ShowNextWindowTab, ShowPreviousWindowTab,
};
use project::{linked_worktree_short_name, repo_identity_path};

#[cfg(not(target_os = "macos"))]
use crate::application_menu::{
    ActivateDirection, ActivateMenuLeft, ActivateMenuRight, OpenApplicationMenu,
};

use auto_update::AutoUpdateStatus;
use call::ActiveCall;
use client::{Client, UserStore, zed_urls};
use command_palette_hooks::CommandPaletteFilter;

use gpui::{
    Action, Anchor, Animation, AnimationExt, AnyElement, App, Context, Element, Entity, Focusable,
    InteractiveElement, IntoElement, MouseButton, ParentElement, Render,
    StatefulInteractiveElement, Styled, Subscription, TaskExt, WeakEntity, Window, actions, div,
    pulsating_between,
};
use onboarding_banner::OnboardingBanner;
use project::{
    Project, git_store::GitStoreEvent, project_settings::ProjectSettings,
    trusted_worktrees::TrustedWorktrees,
};
use remote::RemoteConnectionOptions;
use settings::{Settings as _, SettingsStore};

use std::any::TypeId;
use std::sync::Arc;
use std::time::Duration;
use theme::ActiveTheme;
use title_bar_settings::TitleBarSettings;
use ui::{
    Avatar, ButtonLike, ContextMenu, ContextMenuEntry, IconWithIndicator, Indicator, PopoverMenu,
    PopoverMenuHandle, TintColor, Tooltip, prelude::*, utils::platform_title_bar_height,
};
use update_version::UpdateVersion;
use util::ResultExt;
use workspace::{
    AccessibleMode, MultiWorkspace, Toast, ToggleWorktreeSecurity, Workspace,
    notifications::{NotificationId, NotifyResultExt, NotifyTaskExt as _},
};

use zed_actions::OpenRemote;

pub use onboarding_banner::restore_banner;

const MAX_PROJECT_NAME_LENGTH: usize = 40;
const MAX_BRANCH_NAME_LENGTH: usize = 40;
const MAX_SHORT_SHA_LENGTH: usize = 8;

actions!(
    collab,
    [
        /// Toggles the user menu dropdown.
        ToggleUserMenu,
        /// Toggles the project menu dropdown.
        ToggleProjectMenu,
        /// Switches to a different git branch.
        SwitchBranch,
        /// A debug action to simulate an update being available to test the update banner UI.
        SimulateUpdateAvailable
    ]
);

actions!(
    workspace,
    [
        /// Switches to the classic, editor-focused panel layout.
        UseClassicLayout,
        /// Switches to the agentic panel layout.
        UseAgenticLayout,
    ]
);

pub fn init(cx: &mut App) {
    platform_title_bar::PlatformTitleBar::init(cx);

    update_layout_action_filter(cx);

    cx.observe_global::<SettingsStore>(update_layout_action_filter)
        .detach();

    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else {
            return;
        };
        let multi_workspace = workspace.multi_workspace().cloned();
        let item = cx.new(|cx| TitleBar::new("title-bar", workspace, multi_workspace, window, cx));
        workspace.set_titlebar_item(item.into(), window, cx);

        workspace.register_action(|_workspace, _: &UseClassicLayout, _window, cx| {
            set_window_layout(WindowLayout::Editor(None), cx);
        });

        workspace.register_action(|_workspace, _: &UseAgenticLayout, _window, cx| {
            set_window_layout(WindowLayout::Agent(None), cx);
        });

        workspace.register_action(|workspace, _: &SimulateUpdateAvailable, _window, cx| {
            if let Some(titlebar) = workspace
                .titlebar_item()
                .and_then(|item| item.downcast::<TitleBar>().ok())
            {
                titlebar.update(cx, |titlebar, cx| {
                    titlebar.toggle_update_simulation(cx);
                });
            }
        });

        #[cfg(not(target_os = "macos"))]
        workspace.register_action(|workspace, action: &OpenApplicationMenu, window, cx| {
            if let Some(titlebar) = workspace
                .titlebar_item()
                .and_then(|item| item.downcast::<TitleBar>().ok())
            {
                titlebar.update(cx, |titlebar, cx| {
                    if let Some(ref menu) = titlebar.application_menu {
                        menu.update(cx, |menu, cx| menu.open_menu(action, window, cx));
                    }
                });
            }
        });

        #[cfg(not(target_os = "macos"))]
        workspace.register_action(|workspace, _: &ActivateMenuRight, window, cx| {
            if let Some(titlebar) = workspace
                .titlebar_item()
                .and_then(|item| item.downcast::<TitleBar>().ok())
            {
                titlebar.update(cx, |titlebar, cx| {
                    if let Some(ref menu) = titlebar.application_menu {
                        menu.update(cx, |menu, cx| {
                            menu.navigate_menus_in_direction(ActivateDirection::Right, window, cx)
                        });
                    }
                });
            }
        });

        #[cfg(not(target_os = "macos"))]
        workspace.register_action(|workspace, _: &ActivateMenuLeft, window, cx| {
            if let Some(titlebar) = workspace
                .titlebar_item()
                .and_then(|item| item.downcast::<TitleBar>().ok())
            {
                titlebar.update(cx, |titlebar, cx| {
                    if let Some(ref menu) = titlebar.application_menu {
                        menu.update(cx, |menu, cx| {
                            menu.navigate_menus_in_direction(ActivateDirection::Left, window, cx)
                        });
                    }
                });
            }
        });
    })
    .detach();
}

/// Hides or shows the panel layout actions in the command palette based on
/// whether AI is currently disabled.
fn update_layout_action_filter(cx: &mut App) {
    let disable_ai = project::DisableAiSettings::get_global(cx).disable_ai;
    let layout_actions = [
        TypeId::of::<UseClassicLayout>(),
        TypeId::of::<UseAgenticLayout>(),
    ];
    CommandPaletteFilter::update_global(cx, |filter, _| {
        if disable_ai {
            filter.hide_action_types(&layout_actions);
        } else {
            filter.show_action_types(layout_actions.iter());
        }
    });
}

fn set_window_layout(layout: WindowLayout, cx: &App) {
    let fs = <dyn fs::Fs>::global(cx);
    drop(AgentSettings::set_layout(layout, fs, cx));
}

pub struct TitleBar {
    platform_titlebar: Entity<PlatformTitleBar>,
    project: Entity<Project>,
    user_store: Entity<UserStore>,
    client: Arc<Client>,
    workspace: WeakEntity<Workspace>,
    multi_workspace: Option<WeakEntity<MultiWorkspace>>,
    application_menu: Option<Entity<ApplicationMenu>>,
    _subscriptions: Vec<Subscription>,
    banner: Option<Entity<OnboardingBanner>>,
    update_version: Entity<UpdateVersion>,
    screen_share_popover_handle: PopoverMenuHandle<ContextMenu>,
    _diagnostics_subscription: Option<gpui::Subscription>,
}

impl Render for TitleBar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.multi_workspace.is_none() {
            if let Some(mw) = self
                .workspace
                .upgrade()
                .and_then(|ws| ws.read(cx).multi_workspace().cloned())
            {
                self.multi_workspace = Some(mw.clone());
                self.platform_titlebar.update(cx, |titlebar, _cx| {
                    titlebar.set_multi_workspace(mw);
                });
            }
        }

        let title_bar_settings = *TitleBarSettings::get_global(cx);
        let button_layout = title_bar_settings.button_layout;
        let is_git_enabled = ProjectSettings::get_global(cx).git.enabled.status;

        let show_menus = show_menus(cx);

        let mut children = <ArrayVec<_, 5>>::new();

        let mut project_name = None;
        let mut repository = None;
        let mut linked_worktree_name = None;
        // Read before `repository` is consumed below, so the top bar's repo
        // button has somewhere to point.
        let mut remote_url: Option<String> = None;
        // A repository with no remote is still a repository: commit and push
        // are meaningful, and only the "open on the web" button is not.
        let mut repository_present = false;
        if let Some(worktree) = self.effective_active_worktree(cx) {
            repository = self.get_repository_for_worktree(&worktree, cx);
            let worktree_abs_path = worktree.read(cx).abs_path();
            project_name = worktree
                .read(cx)
                .root_name()
                .file_name()
                .map(|name| SharedString::from(name.to_string()));
            if let Some(repo) = &repository {
                repository_present = true;
                let snapshot = repo.read(cx).snapshot();
                remote_url = snapshot
                    .remote_origin_url
                    .clone()
                    .or_else(|| snapshot.remote_upstream_url.clone());

                let repo = repo.read(cx);
                linked_worktree_name = repo
                    .main_worktree_abs_path()
                    .and_then(|main_worktree_path| {
                        linked_worktree_short_name(
                            main_worktree_path,
                            repo.work_directory_abs_path.as_ref(),
                        )
                    })
                    .or_else(|| {
                        repo.is_linked_worktree()
                            .then_some(project_name.clone())
                            .flatten()
                    });

                let identity = repo_identity_path(&repo.common_dir_abs_path, repo.path_style);

                let display_name = if identity.extension() == Some(std::ffi::OsStr::new("git")) {
                    identity.file_stem().and_then(|n| n.to_str())
                } else {
                    repo.path_style.file_name(identity)
                };

                if let Some(repo_name) = display_name {
                    let visible_worktrees_in_repo = self.visible_worktrees_in_repository(repo, cx);
                    let name = if visible_worktrees_in_repo == 1 {
                        if let Ok(relative) =
                            worktree_abs_path.strip_prefix(&*repo.work_directory_abs_path)
                        {
                            if relative.as_os_str().is_empty() {
                                repo_name.to_string()
                            } else {
                                format!("{}/{}", repo_name, relative.display())
                            }
                        } else {
                            repo_name.to_string()
                        }
                    } else {
                        repo_name.to_string()
                    };
                    project_name = Some(SharedString::from(name));
                }
            }
        }

        children.push(
            h_flex()
                .h_full()
                .gap_0p5()
                .map(|title_bar| {
                    let mut render_project_items = title_bar_settings.show_branch_name
                        || title_bar_settings.show_project_items;
                    title_bar
                        .when_some(
                            self.application_menu.clone().filter(|_| !show_menus),
                            |title_bar, menu| {
                                // Hide the project/branch items to make room when the
                                // menu bar is expanded -- except in accessible mode,
                                // where the menu bar is always expanded but those
                                // controls must still remain reachable.
                                render_project_items &= !menu
                                    .update(cx, |menu, cx| menu.all_menus_shown(cx))
                                    || cx.accessible_mode();
                                title_bar.child(menu)
                            },
                        )
                        .children(self.render_restricted_mode(cx))
                        .when(render_project_items, |title_bar| {
                            title_bar
                                .when(title_bar_settings.show_project_items, |title_bar| {
                                    title_bar
                                        .children(self.render_project_host(cx))
                                        .child(self.render_project_name(project_name, window, cx))
                                })
                                .when_some(
                                    repository.filter(|_| is_git_enabled),
                                    |title_bar, repository| {
                                        title_bar.children(self.render_worktree_and_branch(
                                            repository,
                                            linked_worktree_name,
                                            cx,
                                        ))
                                    },
                                )
                        })
                })
                // Appended inside this group rather than pushed as a new child:
                // `children` is an ArrayVec<_, 5> with 4 pushes already, so a
                // fifth would fill it exactly and panic if upstream ever adds one.
                .child(self.render_title_bar_tools())
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .into_any_element(),
        );

        children.push(self.render_collaborator_list(window, cx).into_any_element());

        if title_bar_settings.show_onboarding_banner {
            if let Some(banner) = &self.banner {
                children.push(banner.clone().into_any_element())
            }
        }

        let status = self.client.status();
        let status = &*status.borrow();
        let user = self.user_store.read(cx).current_user();
        let is_signing_in = user.is_none()
            && matches!(
                status,
                client::Status::Authenticating
                    | client::Status::Authenticated
                    | client::Status::Connecting
            );
        let is_signed_out_or_auth_error = user.is_none()
            && matches!(
                status,
                client::Status::SignedOut | client::Status::AuthenticationError
            );

        children.push(
            h_flex()
                .pr_1()
                .gap_1()
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(self.render_call_controls(window, cx))
                .children(self.render_connection_status(status, cx))
                .child(self.update_version.clone())
                .when(
                    user.is_none()
                        && is_signed_out_or_auth_error
                        && TitleBarSettings::get_global(cx).show_sign_in,
                    |this| this.child(self.render_sign_in_button(cx)),
                )
                .when(is_signing_in, |this| {
                    this.child(
                        Label::new("Signing in…")
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .with_animation(
                                "signing-in",
                                Animation::new(Duration::from_secs(2))
                                    .repeat()
                                    .with_easing(pulsating_between(0.4, 0.8)),
                                |label, delta| label.alpha(delta),
                            ),
                    )
                })
                .child(self.render_title_bar_trailing_tools())
                .when(TitleBarSettings::get_global(cx).show_user_menu, |this| {
                    this.child(self.render_user_menu_button(cx))
                })
                .into_any_element(),
        );

        if show_menus {
            // The row is `justify_between`: the first child sits left with the
            // menus, the last against the right edge inside the window
            // controls.
            let has_repository = remote_url.is_some() || repository_present;
            let git_cluster = self.render_top_bar_git(has_repository);
            let extras = self.render_top_bar_extras(remote_url, cx);
            self.platform_titlebar.update(cx, |this, _| {
                this.set_button_layout(button_layout);
                this.set_children(
                    std::iter::once(
                        h_flex()
                            .gap_1()
                            .children(
                                self.application_menu
                                    .clone()
                                    .map(|menu| menu.into_any_element()),
                            )
                            .child(git_cluster)
                            .into_any_element(),
                    )
                    .chain(std::iter::once(extras)),
                );
            });

            let height = platform_title_bar_height(window);
            let title_bar_color = self.platform_titlebar.update(cx, |platform_titlebar, cx| {
                platform_titlebar.title_bar_color(window, cx)
            });

            v_flex()
                .w_full()
                .child(self.platform_titlebar.clone().into_any_element())
                .child(
                    h_flex()
                        .bg(title_bar_color)
                        .h(height)
                        .pl_2()
                        .justify_between()
                        .w_full()
                        .children(children),
                )
                .into_any_element()
        } else {
            self.platform_titlebar.update(cx, |this, _| {
                this.set_button_layout(button_layout);
                this.set_children(children);
            });
            self.platform_titlebar.clone().into_any_element()
        }
    }
}

impl TitleBar {
    pub fn new(
        id: impl Into<ElementId>,
        workspace: &Workspace,
        multi_workspace: Option<WeakEntity<MultiWorkspace>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let project = workspace.project().clone();
        let git_store = project.read(cx).git_store().clone();
        let user_store = workspace.app_state().user_store.clone();
        let client = workspace.app_state().client.clone();
        let active_call = ActiveCall::global(cx);

        let platform_style = PlatformStyle::platform();
        let application_menu = match platform_style {
            PlatformStyle::Mac => {
                if option_env!("ZED_USE_CROSS_PLATFORM_MENU").is_some() {
                    Some(cx.new(|cx| ApplicationMenu::new(window, cx)))
                } else {
                    None
                }
            }
            PlatformStyle::Linux | PlatformStyle::Windows => {
                Some(cx.new(|cx| ApplicationMenu::new(window, cx)))
            }
        };

        let mut subscriptions = Vec::new();
        subscriptions.push(
            cx.observe(&workspace.weak_handle().upgrade().unwrap(), |_, _, cx| {
                cx.notify()
            }),
        );

        subscriptions.push(cx.observe(&active_call, |this, _, cx| this.active_call_changed(cx)));
        subscriptions.push(
            cx.subscribe(&git_store, move |_, _, event, cx| match event {
                GitStoreEvent::ActiveRepositoryChanged(_)
                | GitStoreEvent::RepositoryUpdated(_, _, true) => {
                    cx.notify();
                }
                _ => {}
            }),
        );
        subscriptions.push(cx.observe(&user_store, |_a, _, cx| cx.notify()));
        if let Some(workspace_entity) = workspace.weak_handle().upgrade() {
            subscriptions.push(cx.subscribe(
                &workspace_entity,
                |_, _, event: &workspace::Event, cx| {
                    if matches!(event, workspace::Event::WorktreeCreationChanged) {
                        cx.notify();
                    }
                },
            ));
        }
        subscriptions.push(cx.observe_button_layout_changed(window, |_, _, cx| cx.notify()));
        if let Some(trusted_worktrees) = TrustedWorktrees::try_get_global(cx) {
            subscriptions.push(cx.subscribe(&trusted_worktrees, |_, _, _, cx| {
                cx.notify();
            }));
        }

        let update_version = cx.new(|cx| UpdateVersion::new(cx));
        let platform_titlebar = cx.new(|cx| {
            let mut titlebar = PlatformTitleBar::new(id, cx);
            if let Some(mw) = multi_workspace.clone() {
                titlebar = titlebar.with_multi_workspace(mw);
            }
            titlebar
        });

        let banner = None;

        let mut this = Self {
            platform_titlebar,
            application_menu,
            workspace: workspace.weak_handle(),
            multi_workspace,
            project,
            user_store,
            client,
            _subscriptions: subscriptions,
            banner,
            update_version,
            screen_share_popover_handle: PopoverMenuHandle::default(),
            _diagnostics_subscription: None,
        };

        this.observe_diagnostics(cx);

        this
    }

    fn worktree_count(&self, cx: &App) -> usize {
        self.project.read(cx).visible_worktrees(cx).count()
    }

    fn toggle_update_simulation(&mut self, cx: &mut Context<Self>) {
        self.update_version
            .update(cx, |banner, cx| banner.update_simulation(cx));
        cx.notify();
    }

    /// Returns the worktree to display in the title bar.
    /// - Prefer the worktree owning the project's active repository
    /// - Fall back to the first visible worktree
    pub fn effective_active_worktree(&self, cx: &App) -> Option<Entity<project::Worktree>> {
        let project = self.project.read(cx);

        if let Some(repo) = project.active_repository(cx) {
            let repo = repo.read(cx);
            let repo_path = &repo.work_directory_abs_path;

            for worktree in project.visible_worktrees(cx) {
                let worktree_path = worktree.read(cx).abs_path();
                if worktree_path == *repo_path || worktree_path.starts_with(repo_path.as_ref()) {
                    return Some(worktree);
                }
            }
        }

        project.visible_worktrees(cx).next()
    }

    fn get_repository_for_worktree(
        &self,
        worktree: &Entity<project::Worktree>,
        cx: &App,
    ) -> Option<Entity<project::git_store::Repository>> {
        let project = self.project.read(cx);
        let git_store = project.git_store().read(cx);
        let worktree_path = worktree.read(cx).abs_path();

        git_store
            .repositories()
            .values()
            .filter(|repo| {
                let repo_path = &repo.read(cx).work_directory_abs_path;
                worktree_path == *repo_path || worktree_path.starts_with(repo_path.as_ref())
            })
            .max_by_key(|repo| repo.read(cx).work_directory_abs_path.as_os_str().len())
            .cloned()
    }

    fn visible_worktrees_in_repository(
        &self,
        repository: &project::git_store::Repository,
        cx: &App,
    ) -> usize {
        let repo_path = &repository.work_directory_abs_path;
        self.project
            .read(cx)
            .visible_worktrees(cx)
            .filter(|worktree| {
                let worktree_path = worktree.read(cx).abs_path();
                worktree_path == *repo_path || worktree_path.starts_with(repo_path.as_ref())
            })
            .count()
    }

    fn render_remote_project_connection(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let workspace = self.workspace.clone();

        let options = self.project.read(cx).remote_connection_options(cx)?;
        let host: SharedString = options.display_name().into();

        let (nickname, tooltip_title, icon) = match options {
            RemoteConnectionOptions::Ssh(options) => (
                options.nickname.map(|nick| nick.into()),
                "Remote Project",
                IconName::Server,
            ),
            RemoteConnectionOptions::Wsl(_) => (None, "Remote Project", IconName::Linux),
            RemoteConnectionOptions::Docker(_dev_container_connection) => {
                (None, "Dev Container", IconName::Box)
            }
            #[cfg(any(test, feature = "test-support"))]
            RemoteConnectionOptions::Mock(_) => (None, "Mock Remote Project", IconName::Server),
        };

        let nickname = nickname.unwrap_or_else(|| host.clone());

        let (indicator_color, meta) = match self.project.read(cx).remote_connection_state(cx)? {
            remote::ConnectionState::Connecting => (Color::Info, format!("Connecting to: {host}")),
            remote::ConnectionState::Connected => (Color::Success, format!("Connected to: {host}")),
            remote::ConnectionState::HeartbeatMissed => (
                Color::Warning,
                format!("Connection attempt to {host} missed. Retrying..."),
            ),
            remote::ConnectionState::Reconnecting => (
                Color::Warning,
                format!("Lost connection to {host}. Reconnecting..."),
            ),
            remote::ConnectionState::Disconnected => {
                (Color::Error, format!("Disconnected from {host}"))
            }
        };

        let icon_color = match self.project.read(cx).remote_connection_state(cx)? {
            remote::ConnectionState::Connecting => Color::Info,
            remote::ConnectionState::Connected => Color::Default,
            remote::ConnectionState::HeartbeatMissed => Color::Warning,
            remote::ConnectionState::Reconnecting => Color::Warning,
            remote::ConnectionState::Disconnected => Color::Error,
        };

        let meta = SharedString::from(meta);

        Some(
            PopoverMenu::new("remote-project-menu")
                .menu(move |window, cx| {
                    let workspace_entity = workspace.upgrade()?;
                    let fs = workspace_entity.read(cx).project().read(cx).fs().clone();
                    Some(recent_projects::RemoteServerProjects::popover(
                        fs,
                        workspace.clone(),
                        None,
                        window,
                        cx,
                    ))
                })
                .trigger_with_tooltip(
                    ButtonLike::new("remote_project")
                        .selected_style(ButtonStyle::Tinted(TintColor::Accent))
                        .child(
                            h_flex()
                                .gap_2()
                                .max_w_32()
                                .child(
                                    IconWithIndicator::new(
                                        Icon::new(icon).size(IconSize::Small).color(icon_color),
                                        Some(Indicator::dot().color(indicator_color)),
                                    )
                                    .indicator_border_color(Some(
                                        cx.theme().colors().title_bar_background,
                                    ))
                                    .into_any_element(),
                                )
                                .child(Label::new(nickname).size(LabelSize::Small).truncate()),
                        ),
                    move |_window, cx| {
                        Tooltip::with_meta(
                            tooltip_title,
                            Some(&OpenRemote::default()),
                            meta.clone(),
                            cx,
                        )
                    },
                )
                .anchor(gpui::Anchor::TopLeft)
                .into_any_element(),
        )
    }

    pub fn render_restricted_mode(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let has_restricted_worktrees =
            TrustedWorktrees::has_restricted_worktrees(&self.project.read(cx).worktree_store(), cx);
        if !has_restricted_worktrees {
            return None;
        }

        let button = Button::new("restricted_mode_trigger", "Restricted Mode")
            .style(ButtonStyle::Tinted(TintColor::Warning))
            .label_size(LabelSize::Small)
            .color(Color::Warning)
            .start_icon(
                Icon::new(IconName::Warning)
                    .size(IconSize::Small)
                    .color(Color::Warning),
            )
            .tooltip(|_, cx| {
                Tooltip::with_meta(
                    "You're in Restricted Mode",
                    Some(&ToggleWorktreeSecurity),
                    "Mark this project as trusted and unlock all features",
                    cx,
                )
            })
            .on_click({
                cx.listener(move |this, _, window, cx| {
                    this.workspace
                        .update(cx, |workspace, cx| {
                            workspace.show_worktree_trust_security_modal(true, window, cx)
                        })
                        .log_err();
                })
            });

        if ui::utils::MACOS_SDK_26_OR_LATER {
            // Make up for Tahoe's traffic light buttons having less spacing around them
            Some(div().child(button).ml_0p5().into_any_element())
        } else {
            Some(button.into_any_element())
        }
    }

    pub fn render_project_host(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.project.read(cx).is_via_remote_server() {
            return self.render_remote_project_connection(cx);
        }

        if self.project.read(cx).is_disconnected(cx) {
            return Some(
                Button::new("disconnected", "Disconnected")
                    .disabled(true)
                    .color(Color::Disabled)
                    .label_size(LabelSize::Small)
                    .into_any_element(),
            );
        }

        let host = self.project.read(cx).host()?;
        let host_user = self.user_store.read(cx).get_cached_user(host.user_id)?;
        let participant_index = self
            .user_store
            .read(cx)
            .participant_indices()
            .get(&host_user.legacy_id)?;

        Some(
            Button::new("project_owner_trigger", host_user.username.clone())
                .color(Color::Player(participant_index.0))
                .label_size(LabelSize::Small)
                .tab_index(0isize)
                .tooltip(move |_, cx| {
                    let tooltip_title = format!(
                        "{} is sharing this project. Click to follow.",
                        host_user.username
                    );

                    Tooltip::with_meta(tooltip_title, None, "Click to Follow", cx)
                })
                .on_click({
                    let host_peer_id = host.peer_id;
                    cx.listener(move |this, _, window, cx| {
                        this.workspace
                            .update(cx, |workspace, cx| {
                                workspace.follow(host_peer_id, window, cx);
                            })
                            .log_err();
                    })
                })
                .into_any_element(),
        )
    }

    fn render_project_name(
        &self,
        name: Option<SharedString>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let workspace = self.workspace.clone();

        let is_project_selected = name.is_some();

        let display_name = if let Some(ref name) = name {
            util::truncate_and_trailoff(name, MAX_PROJECT_NAME_LENGTH)
        } else {
            "Open Recent Project".to_string()
        };

        let is_sidebar_open = self
            .multi_workspace
            .as_ref()
            .and_then(|mw| mw.upgrade())
            .map(|mw| mw.read(cx).sidebar_open())
            .unwrap_or(false)
            && PlatformTitleBar::is_multi_workspace_enabled(cx);

        let is_threads_list_view_active = self
            .multi_workspace
            .as_ref()
            .and_then(|mw| mw.upgrade())
            .map(|mw| mw.read(cx).is_threads_list_view_active(cx))
            .unwrap_or(false);

        if is_sidebar_open && is_threads_list_view_active {
            return self
                .render_recent_projects_popover(display_name, is_project_selected, cx)
                .into_any_element();
        }

        let focus_handle = workspace
            .upgrade()
            .map(|w| w.read(cx).focus_handle(cx))
            .unwrap_or_else(|| cx.focus_handle());

        let window_project_groups: Vec<_> = self
            .multi_workspace
            .as_ref()
            .and_then(|mw| mw.upgrade())
            .map(|mw| mw.read(cx).project_group_keys())
            .unwrap_or_default();

        PopoverMenu::new("recent-projects-menu")
            .menu(move |window, cx| {
                Some(recent_projects::RecentProjects::popover(
                    workspace.clone(),
                    window_project_groups.clone(),
                    None,
                    focus_handle.clone(),
                    window,
                    cx,
                ))
            })
            .trigger_with_tooltip(
                Button::new("project_name_trigger", display_name)
                    .label_size(LabelSize::Small)
                    .tab_index(0isize)
                    .when(self.worktree_count(cx) > 1, |this| {
                        this.end_icon(
                            Icon::new(IconName::ChevronDown)
                                .size(IconSize::XSmall)
                                .color(Color::Muted),
                        )
                    })
                    .selected_style(ButtonStyle::Tinted(TintColor::Accent))
                    .when(!is_project_selected, |s| s.color(Color::Muted)),
                move |_window, cx| {
                    Tooltip::for_action("Recent Projects", &zed_actions::OpenRecent::default(), cx)
                },
            )
            .anchor(gpui::Anchor::TopLeft)
            .into_any_element()
    }

    fn render_recent_projects_popover(
        &self,
        display_name: String,
        is_project_selected: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let workspace = self.workspace.clone();

        let focus_handle = workspace
            .upgrade()
            .map(|w| w.read(cx).focus_handle(cx))
            .unwrap_or_else(|| cx.focus_handle());

        let window_project_groups: Vec<_> = self
            .multi_workspace
            .as_ref()
            .and_then(|mw| mw.upgrade())
            .map(|mw| mw.read(cx).project_group_keys())
            .unwrap_or_default();

        PopoverMenu::new("sidebar-title-recent-projects-menu")
            .menu(move |window, cx| {
                Some(recent_projects::RecentProjects::popover(
                    workspace.clone(),
                    window_project_groups.clone(),
                    None,
                    focus_handle.clone(),
                    window,
                    cx,
                ))
            })
            .trigger_with_tooltip(
                Button::new("project_name_trigger", display_name)
                    .label_size(LabelSize::Small)
                    .tab_index(0isize)
                    .when(self.worktree_count(cx) > 1, |this| {
                        this.end_icon(
                            Icon::new(IconName::ChevronDown)
                                .size(IconSize::XSmall)
                                .color(Color::Muted),
                        )
                    })
                    .selected_style(ButtonStyle::Tinted(TintColor::Accent))
                    .when(!is_project_selected, |s| s.color(Color::Muted)),
                move |_window, cx| {
                    Tooltip::for_action("Recent Projects", &zed_actions::OpenRecent::default(), cx)
                },
            )
            .anchor(gpui::Anchor::TopLeft)
    }

    /// Stage-and-commit and push, on the window frame.
    ///
    /// The two commands a working day is made of, one click each, always in the
    /// same place. They are dispatched by name rather than by importing
    /// `git_ui`, which is not a dependency of this crate and would pull a large
    /// subtree into its rebuild graph for two buttons.
    ///
    /// Commit stages everything first. A commit button that commits nothing
    /// because the changes are unstaged is a button that appears broken, and
    /// the panel is one click away for anyone who wants to stage selectively.
    fn render_top_bar_git(&self, has_repository: bool) -> AnyElement {
        fn dispatch(
            name: &'static str,
        ) -> impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static {
            move |_, window, cx| {
                if let Some(action) = cx.build_action(name, None).log_err() {
                    window.dispatch_action(action, cx);
                }
            }
        }

        // No panel toggle here: the rail above the file tree opens git, and a
        // third way to reach the same panel was clutter. Commit and push stay,
        // because they act rather than open.
        h_flex()
            .gap_0p5()
            .pl_2()
            .child(
                IconButton::new("top-bar-git-commit", IconName::GitBranch)
                    .icon_size(IconSize::XSmall)
                    .icon_color(Color::Muted)
                    .disabled(!has_repository)
                    .tooltip(Tooltip::text("Stage all and commit"))
                    .on_click(|_, window, cx| {
                        if let Some(stage) = cx.build_action("git::StageAll", None).log_err() {
                            window.dispatch_action(stage, cx);
                        }
                        if let Some(commit) = cx.build_action("git::Commit", None).log_err() {
                            window.dispatch_action(commit, cx);
                        }
                    }),
            )
            .child(
                IconButton::new("top-bar-git-push", IconName::ArrowUp)
                    .icon_size(IconSize::XSmall)
                    .icon_color(Color::Muted)
                    .disabled(!has_repository)
                    .tooltip(Tooltip::text("Push"))
                    .on_click(dispatch("git::Push")),
            )
            .into_any_element()
    }

    /// Where feedback goes. The button says so if this is ever emptied, rather
    /// than doing nothing when clicked.
    const FEATURE_REQUEST_URL: &'static str = "https://acuto.dev/feedback";

    /// The window-frame row: a link to the project's repository and a way to
    /// ask for something.
    ///
    /// Up here rather than beside the git and terminal buttons because neither
    /// is about the file you are editing. They are about the project and about
    /// the editor, which is what this row is already for.
    fn render_top_bar_extras(&self, remote_url: Option<String>, _cx: &App) -> AnyElement {
        // Always present, so it is somewhere rather than somewhere-conditional.
        // A project with no remote gets the generic mark, disabled, saying why:
        // a button that vanishes reads as a bug, and this one was reported as
        // one twice.
        let web_url = remote_url.and_then(|url| web_url_for_remote(&url));
        let repository_button = match web_url {
            Some(web_url) => {
                let icon = hosting_icon_for(&web_url);
                let label = SharedString::from(format!("Open {web_url}"));
                IconButton::new("top-bar-repository", icon)
                    .icon_size(IconSize::Small)
                    .icon_color(Color::Muted)
                    .tooltip(Tooltip::text(label))
                    .on_click(move |_, _, cx| cx.open_url(&web_url))
            }
            // The mark stays put whether or not there is a remote: it is where
            // people look for the repository, and a control that appears and
            // disappears is one nobody learns the position of.
            None => IconButton::new("top-bar-repository", IconName::Github)
                .icon_size(IconSize::Small)
                .icon_color(Color::Disabled)
                .disabled(true)
                .tooltip(Tooltip::text("This project has no git remote to open")),
        };

        let has_destination = !Self::FEATURE_REQUEST_URL.is_empty();
        let workspace = self.workspace.clone();
        let feature_request = IconButton::new("top-bar-feature-request", IconName::Star)
            .icon_size(IconSize::Small)
            .icon_color(Color::Muted)
            .tooltip(Tooltip::text(if has_destination {
                "Feedback"
            } else {
                "Feedback - not open yet"
            }))
            .on_click(move |_, _, cx| {
                if has_destination {
                    cx.open_url(Self::FEATURE_REQUEST_URL);
                    return;
                }
                // A button that does nothing when clicked reads as broken. It
                // says why instead, and becomes a link the moment the constant
                // above has a value.
                workspace
                    .update(cx, |workspace, cx| {
                        workspace.show_toast(
                            Toast::new(
                                NotificationId::unique::<FeatureRequestUnavailable>(),
                                "Feedback is not open yet.",
                            ),
                            cx,
                        );
                    })
                    .log_err();
            });

        h_flex()
            .gap_0p5()
            .pr_2()
            .child(repository_button)
            .child(feature_request)
            .into_any_element()
    }

    /// Git and terminal controls in the title bar.
    ///
    /// Actions are dispatched by name through the global registry rather than by
    /// importing `git_ui` and `terminal_view`. Neither is a dependency of this
    /// crate, and adding them would pull two large subtrees into its rebuild
    /// graph for the sake of two buttons. `build_action` returns a `Result`, so
    /// an action that is renamed upstream degrades to a logged warning and a
    /// dead button rather than a panic.
    fn render_title_bar_tools(&self) -> impl IntoElement {
        h_flex()
            .gap_0p5()
            // First, ahead of git: the tree is the control you reach for most,
            // and it used to sit in the status bar where it was the only
            // navigation control not on this row.
            .child(
                IconButton::new("title-bar-project-panel", IconName::FileTree)
                    .tooltip(Tooltip::text("Project Panel"))
                    .icon_size(IconSize::Small)
                    .on_click(move |_, window, cx| {
                        if let Some(action) = cx
                            .build_action("project_panel::ToggleFocus", None)
                            .log_err()
                        {
                            window.dispatch_action(action, cx);
                        }
                    }),
            )
            .child(
                IconButton::new("title-bar-terminal", IconName::Terminal)
                    .tooltip(Tooltip::text("Terminal"))
                    .icon_size(IconSize::Small)
                    .on_click(move |_, window, cx| {
                        if let Some(action) =
                            cx.build_action("terminal_panel::Toggle", None).log_err()
                        {
                            window.dispatch_action(action, cx);
                        }
                    }),
            )
    }

    /// Agent toggle and settings, rendered on the trailing side of the bar.
    ///
    /// Split from the git and terminal controls deliberately: those relate to the
    /// project shown on the left, while these are application-level and belong
    /// with the other trailing controls.
    fn render_title_bar_trailing_tools(&self) -> impl IntoElement {
        h_flex()
            .gap_0p5()
            // No agent button here. The agent panel already has one in the
            // status bar's dock strip, which is where every other panel in the
            // window is toggled from -- two controls for one panel, at opposite
            // corners, is a thing to hunt for rather than a shortcut.
            // Team messages and tickets, on the right with the other things
            // that are about the project rather than the file. They open as
            // a tab, so they have no dock button; this is the way in.
            .child(
                IconButton::new("title-bar-team", IconName::ListTodo)
                    .tooltip(Tooltip::text("Team \u{2014} messages and tickets"))
                    .icon_size(IconSize::Small)
                    .on_click(move |_, window, cx| {
                        if let Some(action) =
                            cx.build_action("team_notes::ToggleFocus", None).log_err()
                        {
                            window.dispatch_action(action, cx);
                        }
                    }),
            )
            .child(
                IconButton::new("title-bar-settings", IconName::Settings)
                    .tooltip(Tooltip::text("Settings"))
                    .icon_size(IconSize::Small)
                    .on_click(move |_, window, cx| {
                        if let Some(action) = cx.build_action("zed::OpenSettings", None).log_err() {
                            window.dispatch_action(action, cx);
                        }
                    }),
            )
    }

    fn render_worktree_and_branch(
        &self,
        repository: Entity<project::git_store::Repository>,
        linked_worktree_name: Option<SharedString>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let workspace = self.workspace.upgrade()?;

        let (branch_name, icon_info, is_detached_head) = {
            let repo = repository.read(cx);

            let is_detached_head = repo.branch.is_none();

            let branch_name = repo
                .branch
                .as_ref()
                .map(|branch| branch.name())
                .map(|name| util::truncate_and_trailoff(name, MAX_BRANCH_NAME_LENGTH))
                .or_else(|| {
                    repo.head_commit.as_ref().map(|commit| {
                        commit
                            .sha
                            .chars()
                            .take(MAX_SHORT_SHA_LENGTH)
                            .collect::<String>()
                    })
                });

            let status = repo.status_summary();
            let tracked = status.index + status.worktree;
            let icon_info = if status.conflict > 0 {
                (IconName::Warning, Color::VersionControlConflict)
            } else if tracked.modified > 0 {
                (IconName::SquareDot, Color::VersionControlModified)
            } else if tracked.added > 0 || status.untracked > 0 {
                (IconName::SquarePlus, Color::VersionControlAdded)
            } else if tracked.deleted > 0 {
                (IconName::SquareMinus, Color::VersionControlDeleted)
            } else {
                (IconName::GitBranch, Color::Muted)
            };

            (branch_name, icon_info, is_detached_head)
        };

        let settings = TitleBarSettings::get_global(cx);
        let effective_repository = Some(repository);

        let worktree_label: SharedString = linked_worktree_name.unwrap_or_else(|| "main".into());

        let (creation_in_progress, is_switch) = self
            .workspace
            .upgrade()
            .map(|ws| {
                let creation = ws.read(cx).active_worktree_creation();
                (creation.label.clone(), creation.is_switch)
            })
            .unwrap_or((None, false));
        let is_creating = creation_in_progress.is_some();

        let display_label: SharedString = if let Some(ref name) = creation_in_progress {
            if is_switch {
                format!("Loading {}…", name).into()
            } else {
                format!("Creating {}…", name).into()
            }
        } else {
            worktree_label.clone()
        };

        let worktree_button = settings.show_worktree_name.then(|| {
            let project = self.project.clone();
            let workspace_handle = workspace.downgrade();
            PopoverMenu::new("worktree-picker-menu")
                .menu(move |window, cx| {
                    // When opened from the title bar, focus is on the trigger
                    // button (not a dock), so `focused_dock` is `None`. That's
                    // fine — there's no prior dock focus to restore.
                    Some(cx.new(|cx| {
                        WorktreePicker::new(project.clone(), workspace_handle.clone(), window, cx)
                    }))
                })
                .trigger_with_tooltip(
                    Button::new("worktree_picker_trigger", display_label)
                        .selected_style(ButtonStyle::Tinted(TintColor::Accent))
                        .label_size(LabelSize::Small)
                        .color(Color::Muted)
                        .tab_index(0isize)
                        .loading(is_creating)
                        .start_icon(
                            Icon::new(IconName::GitWorktree)
                                .size(IconSize::XSmall)
                                .color(Color::Muted),
                        ),
                    move |_window, cx| {
                        Tooltip::with_meta(
                            "Worktree",
                            Some(&zed_actions::git::Worktree),
                            format!("Currently In Use: {}", worktree_label),
                            cx,
                        )
                    },
                )
                .anchor(gpui::Anchor::TopLeft)
        });

        let branch_picker = branch_name.and_then(|branch_name| {
            settings.show_branch_name.then(|| {
                let branch_tooltip_label = branch_name.clone();
                let (branch_icon, branch_icon_color) = if settings.show_branch_status_icon {
                    icon_info
                } else {
                    (IconName::GitBranch, Color::Muted)
                };

                let trigger = if is_detached_head {
                    Button::new("project_branch_trigger", "Create Branch")
                        .selected_style(ButtonStyle::Tinted(TintColor::Accent))
                        .label_size(LabelSize::Small)
                        .tab_index(0isize)
                        .start_icon(
                            Icon::new(IconName::GitBranchPlus)
                                .size(IconSize::XSmall)
                                .color(Color::Muted),
                        )
                } else {
                    Button::new("project_branch_trigger", branch_name)
                        .selected_style(ButtonStyle::Tinted(TintColor::Accent))
                        .label_size(LabelSize::Small)
                        .color(Color::Muted)
                        .tab_index(0isize)
                        .start_icon(
                            Icon::new(branch_icon)
                                .size(IconSize::XSmall)
                                .color(branch_icon_color),
                        )
                };

                PopoverMenu::new("branch-menu")
                    .menu(move |window, cx| {
                        git_ui_core::build_branch_picker(
                            workspace.downgrade(),
                            effective_repository.clone(),
                            window,
                            cx,
                        )
                    })
                    .trigger_with_tooltip(trigger, move |_window, cx| {
                        let meta = if is_detached_head {
                            format!("Detached HEAD: {}", branch_tooltip_label)
                        } else {
                            format!("Currently Checked Out: {}", branch_tooltip_label)
                        };
                        Tooltip::with_meta(
                            "Branch & Stash",
                            Some(&zed_actions::git::Branch),
                            meta,
                            cx,
                        )
                    })
                    .anchor(gpui::Anchor::TopLeft)
            })
        });

        if worktree_button.is_none() && branch_picker.is_none() {
            return None;
        }

        let show_separator = worktree_button.is_some() && branch_picker.is_some();

        Some(
            h_flex()
                .gap_px()
                .children(worktree_button)
                .when(show_separator, |this| {
                    this.child(
                        Label::new("/")
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .alpha(0.25),
                    )
                })
                .children(branch_picker)
                .into_any_element(),
        )
    }

    fn active_call_changed(&mut self, cx: &mut Context<Self>) {
        self.observe_diagnostics(cx);
        cx.notify();
    }

    fn observe_diagnostics(&mut self, cx: &mut Context<Self>) {
        let diagnostics = ActiveCall::global(cx)
            .read(cx)
            .room()
            .and_then(|room| room.read(cx).diagnostics().cloned());

        if let Some(diagnostics) = diagnostics {
            self._diagnostics_subscription = Some(cx.observe(&diagnostics, |_, _, cx| cx.notify()));
        } else {
            self._diagnostics_subscription = None;
        }
    }

    fn share_project(&mut self, cx: &mut Context<Self>) {
        let active_call = ActiveCall::global(cx);
        let project = self.project.clone();
        active_call
            .update(cx, |call, cx| call.share_project(project, cx))
            .detach_and_log_err(cx);
    }

    fn unshare_project(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        let active_call = ActiveCall::global(cx);
        let project = self.project.clone();
        active_call
            .update(cx, |call, cx| call.unshare_project(project, cx))
            .log_err();
    }

    fn render_connection_status(
        &self,
        status: &client::Status,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        match status {
            client::Status::ConnectionError
            | client::Status::ConnectionLost
            | client::Status::Reauthenticating
            | client::Status::Reconnecting
            | client::Status::ReconnectionError { .. } => Some(
                div()
                    .id("disconnected")
                    .child(Icon::new(IconName::Disconnected).size(IconSize::Small))
                    .tooltip(Tooltip::text("Disconnected"))
                    .into_any_element(),
            ),
            client::Status::UpgradeRequired => {
                let auto_updater = auto_update::AutoUpdater::get(cx);
                let label = match auto_updater.map(|auto_update| auto_update.read(cx).status()) {
                    Some(AutoUpdateStatus::Updated { .. }) => "Please restart Acuto to Collaborate",
                    Some(AutoUpdateStatus::Installing { .. })
                    | Some(AutoUpdateStatus::Downloading { .. })
                    | Some(AutoUpdateStatus::Checking) => "Updating...",
                    Some(AutoUpdateStatus::Idle)
                    | Some(AutoUpdateStatus::Errored { .. })
                    | None => "Please update Acuto to Collaborate",
                };

                Some(
                    Button::new("connection-status", label)
                        .label_size(LabelSize::Small)
                        .on_click(|_, window, cx| {
                            if let Some(auto_updater) = auto_update::AutoUpdater::get(cx)
                                && auto_updater.read(cx).status().is_updated()
                            {
                                workspace::reload(cx);
                                return;
                            }
                            auto_update::check(&Default::default(), window, cx);
                        })
                        .into_any_element(),
                )
            }
            _ => None,
        }
    }

    pub fn render_sign_in_button(&mut self, _: &mut Context<Self>) -> Button {
        let client = self.client.clone();
        let workspace = self.workspace.clone();
        Button::new("sign_in", "Sign In")
            .label_size(LabelSize::Small)
            .tab_index(0isize)
            .on_click(move |_, window, cx| {
                let client = client.clone();
                let workspace = workspace.clone();
                window
                    .spawn(cx, async move |mut cx| {
                        client
                            .sign_in_with_optional_connect(true, cx)
                            .await
                            .notify_workspace_async_err(workspace, &mut cx);
                    })
                    .detach();
            })
    }

    pub fn render_user_menu_button(&mut self, cx: &mut Context<Self>) -> impl Element {
        let show_update_button = self.update_version.read(cx).show_update_in_menu_bar();

        let user_store = self.user_store.clone();
        let workspace = self.workspace.clone();
        let user = user_store.read(cx).current_user();

        let user_avatar = user.as_ref().map(|u| u.avatar_uri.clone());
        let username = user.as_ref().map(|u| u.username.clone());

        let is_signed_in = user.is_some();

        let current_organization = user_store.read(cx).current_organization();
        let business_organization = current_organization
            .as_ref()
            .filter(|organization| !organization.is_personal);
        let organizations: Vec<_> = user_store
            .read(cx)
            .organizations()
            .iter()
            .map(|organization| {
                let plan = user_store.read(cx).plan_for_organization(&organization.id);
                (organization.clone(), plan)
            })
            .collect();

        let show_user_picture = TitleBarSettings::get_global(cx).show_user_picture;

        let trigger = if is_signed_in && show_user_picture {
            let avatar = user_avatar.map(|avatar| Avatar::new(avatar)).map(|avatar| {
                if show_update_button {
                    avatar.indicator(
                        div()
                            .absolute()
                            .bottom_0()
                            .right_0()
                            .child(Indicator::dot().color(Color::Accent)),
                    )
                } else {
                    avatar
                }
            });

            ButtonLike::new("user-menu")
                .aria_label("User menu")
                .tab_index(0isize)
                .child(
                    h_flex()
                        .when_some(business_organization, |this, organization| {
                            this.gap_2()
                                .child(Label::new(&organization.name).size(LabelSize::Small))
                        })
                        .children(avatar),
                )
        } else {
            ButtonLike::new("user-menu")
                .aria_label("User menu")
                .tab_index(0isize)
                .child(Icon::new(IconName::ChevronDown).size(IconSize::Small))
        };

        PopoverMenu::new("user-menu")
            .trigger(trigger)
            .menu(move |window, cx| {
                let username = username.clone();
                let current_organization = current_organization.clone();
                let organizations = organizations.clone();
                let user_store = user_store.clone();
                let workspace = workspace.clone();

                let ai_enabled = !project::DisableAiSettings::get_global(cx).disable_ai;
                let current_layout = AgentSettings::get_layout(cx);
                let is_editor = matches!(current_layout, WindowLayout::Editor(_));
                let is_agent = matches!(current_layout, WindowLayout::Agent(_));
                let is_custom = matches!(current_layout, WindowLayout::Custom(_));

                ContextMenu::build(window, cx, |menu, _, _cx| {
                    menu.when(is_signed_in, |this| {
                        let username = username.clone();
                        this.custom_entry(
                            move |_window, _cx| {
                                let username = username.clone().unwrap_or_default();

                                h_flex()
                                    .w_full()
                                    .justify_between()
                                    .child(Label::new(username))
                                    .into_any_element()
                            },
                            move |_, cx| {
                                cx.open_url(&zed_urls::account_url(cx));
                            },
                        )
                        .separator()
                    })
                    .when(show_update_button, |this| {
                        this.custom_entry(
                            move |_window, _cx| {
                                h_flex()
                                    .w_full()
                                    .gap_1()
                                    .justify_between()
                                    .child(
                                        Label::new("Restart to update Acuto").color(Color::Accent),
                                    )
                                    .child(
                                        Icon::new(IconName::Download)
                                            .size(IconSize::Small)
                                            .color(Color::Accent),
                                    )
                                    .into_any_element()
                            },
                            move |_, cx| {
                                workspace::reload(cx);
                            },
                        )
                        .separator()
                    })
                    .map(|this| {
                        let mut this = this.header("Organization");

                        for (organization, plan) in &organizations {
                            let organization = organization.clone();
                            let plan = *plan;

                            let is_current =
                                current_organization
                                    .as_ref()
                                    .is_some_and(|current_organization| {
                                        current_organization.id == organization.id
                                    });

                            this = this.custom_entry(
                                {
                                    let organization = organization.clone();
                                    move |_window, _cx| {
                                        h_flex()
                                            .w_full()
                                            .gap_4()
                                            .justify_between()
                                            .child(
                                                h_flex()
                                                    .gap_1()
                                                    .child(Label::new(&organization.name))
                                                    .when(is_current, |this| {
                                                        this.child(
                                                            Icon::new(IconName::Check)
                                                                .color(Color::Accent),
                                                        )
                                                    }),
                                            )
                                            .children(plan.map(|plan| PlanChip::new(plan)))
                                            .into_any_element()
                                    }
                                },
                                {
                                    let user_store = user_store.clone();
                                    let organization = organization.clone();
                                    let workspace = workspace.clone();
                                    move |window, cx| {
                                        let task = user_store.update(cx, |user_store, cx| {
                                            user_store
                                                .set_current_organization(organization.clone(), cx)
                                        });
                                        task.detach_and_notify_err(workspace.clone(), window, cx);
                                    }
                                },
                            );
                        }

                        this.separator()
                    })
                    .action("Settings", zed_actions::OpenSettings.boxed_clone())
                    .action("Keymap", Box::new(zed_actions::OpenKeymap))
                    .action(
                        "Themes…",
                        zed_actions::theme_selector::Toggle::default().boxed_clone(),
                    )
                    .action(
                        "Icon Themes…",
                        zed_actions::icon_theme_selector::Toggle::default().boxed_clone(),
                    )
                    .action(
                        "Extensions",
                        zed_actions::Extensions::default().boxed_clone(),
                    )
                    .when(ai_enabled, |menu| {
                        menu.separator()
                            .submenu("Panel Layout", move |menu, _window, _cx| {
                                menu.toggleable_entry(
                                    "Classic",
                                    is_editor,
                                    IconPosition::Start,
                                    Some(UseClassicLayout.boxed_clone()),
                                    move |window, cx| {
                                        window.dispatch_action(UseClassicLayout.boxed_clone(), cx);
                                    },
                                )
                                .toggleable_entry(
                                    "Agentic",
                                    is_agent,
                                    IconPosition::Start,
                                    Some(UseAgenticLayout.boxed_clone()),
                                    move |window, cx| {
                                        window.dispatch_action(UseAgenticLayout.boxed_clone(), cx);
                                    },
                                )
                                .when(is_custom, |menu| {
                                    menu.item(
                                        ContextMenuEntry::new("Custom")
                                            .toggleable(IconPosition::Start, true)
                                            .disabled(true),
                                    )
                                })
                            })
                    })
                    .when(is_signed_in, |this| {
                        this.separator()
                            .action("Sign Out", client::SignOut.boxed_clone())
                    })
                })
                .into()
            })
            .anchor(Anchor::TopRight)
    }
}

/// A notification id for the "no feature tracker yet" toast.
struct FeatureRequestUnavailable;

/// Turns a git remote into something a browser can open.
///
/// Handles the two forms a remote actually takes -- `https://host/owner/repo`
/// and `git@host:owner/repo.git` -- and refuses anything else rather than
/// guessing, because a wrong URL opens a browser tab at a stranger's project.
fn web_url_for_remote(remote: &str) -> Option<String> {
    let remote = remote.trim().trim_end_matches('/');
    let remote = remote.strip_suffix(".git").unwrap_or(remote);

    if let Some(rest) = remote.strip_prefix("git@") {
        // `git@host:owner/repo`. The colon is a separator here, not a port.
        let (host, path) = rest.split_once(':')?;
        return Some(format!("https://{host}/{}", path.trim_start_matches('/')));
    }

    if let Some(rest) = remote.strip_prefix("ssh://git@") {
        return Some(format!("https://{rest}"));
    }

    if remote.starts_with("https://") || remote.starts_with("http://") {
        return Some(remote.to_string());
    }

    None
}

/// Picks the forge's own mark when the host is one we can name, and a generic
/// link when it is not -- a self-hosted GitLab is not going to be recognised by
/// its hostname, and showing GitHub's mark for it would be worse than neutral.
fn hosting_icon_for(web_url: &str) -> IconName {
    let host = web_url
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();

    match host.as_str() {
        "github.com" | "www.github.com" => IconName::Github,
        "gitlab.com" | "www.gitlab.com" => IconName::Gitlab,
        "bitbucket.org" => IconName::Bitbucket,
        "codeberg.org" => IconName::Codeberg,
        _ => IconName::Link,
    }
}

#[cfg(test)]
mod acuto_tests {
    use super::*;

    #[test]
    fn remotes_become_browsable_urls() {
        assert_eq!(
            web_url_for_remote("git@github.com:acuto/acuto.git").as_deref(),
            Some("https://github.com/acuto/acuto")
        );
        assert_eq!(
            web_url_for_remote("https://gitlab.com/group/project.git").as_deref(),
            Some("https://gitlab.com/group/project")
        );
        assert_eq!(
            web_url_for_remote("ssh://git@git.example.com/team/repo.git").as_deref(),
            Some("https://git.example.com/team/repo")
        );
        // Not a form we can convert, so it gets no button rather than a wrong one.
        assert_eq!(web_url_for_remote("/srv/git/repo.git"), None);
        assert_eq!(web_url_for_remote(""), None);
    }

    #[test]
    fn only_recognised_hosts_get_their_own_mark() {
        assert_eq!(hosting_icon_for("https://github.com/a/b"), IconName::Github);
        assert_eq!(hosting_icon_for("https://gitlab.com/a/b"), IconName::Gitlab);
        assert_eq!(
            hosting_icon_for("https://git.internal.example/a/b"),
            IconName::Link
        );
    }
}
