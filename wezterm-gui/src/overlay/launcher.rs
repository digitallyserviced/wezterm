//! The launcher is a menu that presents a list of activities that can
//! be launched, such as spawning a new tab in various domains or attaching
//! ssh/tls domains.
//! The launcher is implemented here as an overlay, but could potentially
//! be rendered as a popup/context menu if the system supports it; at the
//! time of writing our window layer doesn't provide an API for context
//! menus.
use crate::commands::ExpandedCommand;
use crate::inputmap::InputMap;
use crate::termwindow::TermWindowNotif;
use async_trait::async_trait;
use config::configuration;
use config::keyassignment::{KeyAssignment, KeyTableEntry, SpawnCommand, SpawnTabDomain};

use downcast_rs::{impl_downcast, Downcast};
use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use luahelper::impl_lua_conversion_dynamic;

use mux::domain::{Domain, DomainId, DomainState};
use mux::pane::PaneId;
use mux::tab::{Tab, TabId};
use mux::termwiztermtab::TermWizTerminal;
use mux::window::WindowId;
use mux::Mux;
use std::borrow::Borrow;

use termwiz::cell::{AttributeChange, CellAttributes};
use termwiz::color::ColorAttribute;
use termwiz::input::{InputEvent, KeyCode, KeyEvent, Modifiers, MouseButtons, MouseEvent};
use termwiz::surface::{Change, Position};
use termwiz::terminal::Terminal;
use termwiz_funcs::truncate_right;
use wezterm_dynamic::{FromDynamic, ToDynamic};

use window::WindowOps;

pub use config::keyassignment::LauncherFlags;

#[cfg_attr(feature = "use_serde", derive(Serialize, Deserialize))]
#[derive(Clone, Debug, ToDynamic, FromDynamic)]
pub enum LauncherEntryType {
    Tab(LauncherTabEntry),
    Domain(LauncherDomainEntry),
    KeyAssignment(LauncherKeyEntry),
    Command(LauncherCommandEntry), // Workspace(Entry),
                                   // Normal(Entry),
}
impl_lua_conversion_dynamic!(LauncherEntryType);

#[derive(Clone, Debug)]
struct Entry {
    pub label: String,
    pub action: KeyAssignment,
}

// pub trait LauncherItems {
//     fn get_entries(&self) -> Vec<LauncherEntry>;
// }

#[derive(Clone, Debug, ToDynamic, FromDynamic)]
pub struct LauncherEntry {
    pub label: String,
    pub action: KeyAssignment,
    pub launch_type: LauncherEntryType,
}
impl_lua_conversion_dynamic!(LauncherEntry);

// #[async_trait(?Send)]
impl LauncherEntry {
    pub async fn new(label: String, action: KeyAssignment, launch_type: LauncherEntryType) -> Self {
        let nlabel = config::with_lua_config_on_main_thread(|lua| async {
            let lua = lua.ok_or_else(|| anyhow::anyhow!("missing lua context"))?;
            let value = config::lua::emit_async_callback(
                lua.borrow(),
                (
                    "format-launcher-item".to_string(),
                    (label.clone(), action.clone(), launch_type.clone()),
                ),
            )
            .await?;
            let label: String = luahelper::from_lua_value_dynamic(value)?;
            Ok(label)
        })
        .await;

        match nlabel {
            Ok(nlabel) => Self {
                label: nlabel,
                action,
                launch_type,
            },
            Err(err) => {
                log::error!(
                    "Error while calling label function launcher entry `{}` {:?} {:?}: {err:#}",
                    label,
                    action,
                    launch_type
                );
                Self {
                    label,
                    action,
                    launch_type,
                }
            }
        }
    }

    // async fn format_launcher_item(mut self, _orig: String) {
    //     let label = config::with_lua_config_on_main_thread(|lua| async {
    //         let lua = lua.ok_or_else(|| anyhow::anyhow!("missing lua context"))?;
    //         let value = config::lua::emit_async_callback(
    //             lua.borrow(),
    //             ("format-launcher-item".to_string(), (self.clone(),)),
    //         )
    //         .await?;
    //         let label: String = luahelper::from_lua_value_dynamic(value)?;
    //         Ok(label)
    //     })
    //     .await;
    //     self.set_label(match label {
    //         Ok(label) => label,
    //         Err(err) => {
    //             log::error!(
    //                 "Error while calling label function for ExecDomain `{}`: {err:#}",
    //                 self.label
    //             );
    //             self.label.to_string()
    //         }
    //     })
    // }
    //
    // pub fn set_label(&mut self, label: String) {
    //     self.label = label;
    // }
}
#[derive(Clone, Debug, ToDynamic, FromDynamic)]
pub struct LauncherKeyEntry {
    pub code: String,
    pub mods: String,
    pub assignment: KeyAssignment,
}

#[derive(Clone, Debug, ToDynamic, FromDynamic)]
pub struct LauncherTabEntry {
    pub title: String,
    pub tab_id: TabId,
    pub tab_idx: usize,
    pub pane_count: usize,
}

#[derive(Clone, Debug, ToDynamic, FromDynamic)]
pub struct LauncherDomainEntry {
    pub domain_id: DomainId,
    pub name: String,
    pub state: DomainState,
    pub label: String,
}
impl_lua_conversion_dynamic!(LauncherDomainEntry);

#[async_trait(?Send)]
pub trait LauncherItem: Downcast {
    async fn get_entry(&self, idx: usize) -> LauncherEntry;
    async fn get_label(&self) -> String;
    async fn get_action(&self) -> Option<KeyAssignment>;
}
impl_downcast!(LauncherItem);
#[async_trait(?Send)]
impl LauncherItem for dyn Domain {
    async fn get_entry(&self, idx: usize) -> LauncherEntry {
        let actionn = self.get_action().await.unwrap();
        let label = self.get_label().await;
        LauncherEntry::new(
            label.clone(),
            actionn,
            LauncherEntryType::Domain(LauncherDomainEntry {
                domain_id: self.domain_id(),
                name: self.domain_name().to_string(),
                state: self.state(),
                label: "".to_string(),
            }),
        )
        .await
    }

    async fn get_label(&self) -> String {
        let name = self.domain_name();
        let label = self.domain_label().await;
        let label = if name == label || label == "" {
            format!("domain `{}`", name)
        } else {
            format!("domain `{}` - {}", name, label)
        };
        label
    }

    async fn get_action(&self) -> Option<KeyAssignment> {
        if let DomainState::Attached = self.state() {
            Some(KeyAssignment::SpawnCommandInNewTab(SpawnCommand {
                domain: SpawnTabDomain::DomainName(self.domain_name().to_string()),
                ..SpawnCommand::default()
            }))
        } else {
            Some(KeyAssignment::AttachDomain(self.domain_name().to_string()))
        }
    }
    // add code here
}
// pub type KeyMapEntry = ((String, String), KeyTableEntry);
#[async_trait(?Send)]
impl LauncherItem for ((window::KeyCode, window::Modifiers), KeyTableEntry) {
    async fn get_entry(&self, idx: usize) -> LauncherEntry {
        let code = self.0 .0.clone();
        let mods = self.0 .1.clone();
        let label = self.get_label().await;
        let action = self.get_action().await.unwrap();
        LauncherEntry::new(
            label,
            action.clone(),
            LauncherEntryType::KeyAssignment(LauncherKeyEntry {
                code: code.to_string(),
                mods: mods.to_string(),
                assignment: action.clone(),
            }),
        )
        .await
    }

    async fn get_label(&self) -> String {
        format!(
            "{:?} ({:?} {:?})",
            self.1.action,
            self.0 .1.clone(),
            self.0 .0.clone()
        )
    }

    async fn get_action(&self) -> Option<KeyAssignment> {
        Some(self.1.clone().action)
    }
}
#[async_trait(?Send)]
impl LauncherItem for Tab {
    async fn get_entry(&self, idx: usize) -> LauncherEntry {
        let label = self.get_label().await;
        let action = self.get_action().await.unwrap();
        LauncherEntry::new(
            label,
            action,
            LauncherEntryType::Tab(LauncherTabEntry {
                title: self.get_title(),
                tab_id: self.get_active_idx(),
                tab_idx: idx,
                pane_count: self.count_panes(),
            }),
        )
        .await
    }

    async fn get_label(&self) -> String {
        format!("{}. {} panes", self.get_title(), self.count_panes())
    }

    async fn get_action(&self) -> Option<KeyAssignment> {
        Some(KeyAssignment::ActivateTab(self.tab_id() as isize))
    }
}

#[derive(Clone, Debug, ToDynamic, FromDynamic)]
pub struct LauncherCommandEntry {
    brief: String,
    doc: String,
    keys: String,
    action: KeyAssignment,
}
#[async_trait(?Send)]
impl LauncherItem for ExpandedCommand {
    async fn get_entry(&self, idx: usize) -> LauncherEntry {
        let label = self.get_label().await;
        let action = self.get_action().await.unwrap();
        LauncherEntry::new(
            label,
            action.clone(),
            LauncherEntryType::Command(LauncherCommandEntry {
                brief: self.brief.to_string(),
                doc: self.doc.to_string(),
                keys: format!("{:?}", self.keys).to_string(),
                action: action.clone(),
            }),
        )
        .await
    }

    async fn get_label(&self) -> String {
        format!("{}. {}", self.brief, self.doc)
    }

    async fn get_action(&self) -> Option<KeyAssignment> {
        Some(self.action.clone())
    }
}
pub struct LauncherArgs {
    flags: LauncherFlags,
    domains: Vec<LauncherEntry>,
    cmddefs: Vec<LauncherEntry>,
    shortcuts: Vec<LauncherEntry>,
    tabs: Vec<LauncherEntry>,
    entries: Vec<LauncherEntry>,
    pane_id: PaneId,
    domain_id_of_current_tab: DomainId,
    title: String,
    active_workspace: String,
    workspaces: Vec<String>,
}

impl LauncherArgs {
    /// Must be called on the Mux thread!
    pub async fn new(
        title: &str,
        flags: LauncherFlags,
        mux_window_id: WindowId,
        pane_id: PaneId,
        domain_id_of_current_tab: DomainId,
    ) -> Self {
        let mux = Mux::get().unwrap();

        let active_workspace = mux.active_workspace();

        let workspaces = if flags.contains(LauncherFlags::WORKSPACES) {
            mux.iter_workspaces()
        } else {
            vec![]
        };
        let config = configuration();
        let mut cmddefs = vec![];
        if flags.contains(LauncherFlags::COMMANDS) {
            let commands = crate::commands::CommandDef::expanded_commands(&config);
            for cmd in commands {
                match &cmd.action {
                    KeyAssignment::ActivateTabRelative(_) | KeyAssignment::ActivateTab(_) => {
                        continue
                    }
                    _ => {
                        let entry = cmd.get_entry(0).await;
                        cmddefs.push(entry);
                    }
                }
            }
        }

        let mut key_entries: Vec<LauncherEntry> = vec![];
        if flags.contains(LauncherFlags::KEY_ASSIGNMENTS) {
            let input_map = InputMap::new(&config);
            // Give a consistent order to the entries
            let keys: Vec<((window::KeyCode, window::Modifiers), KeyTableEntry)> =
                input_map.keys.default.into_iter().collect();
            // let keys: BTreeMap<_, _> = input_map.keys.default.into_iter().collect();
            // for ((keycode, mods), entry) in keys {
            for keymap in keys {
                let entry = match keymap.get_action().await.unwrap() {
                    KeyAssignment::ActivateTabRelative(_) | KeyAssignment::ActivateTab(_) => {
                        continue
                    }
                    action => match key_entries.iter().find(|ent| ent.action == action) {
                        Some(_) => continue,
                        None => keymap.get_entry(0 as usize).await,
                    },
                };
                key_entries.push(entry);
            }
            key_entries.sort_by(|a, b| a.label.cmp(&b.label));
        }
        let entries = vec![];
        let tabs = if flags.contains(LauncherFlags::TABS) {
            let mut otabs = vec![];
            let window = mux
                .get_window(mux_window_id)
                .expect("to resolve my own window_id");
            let ttabs: Vec<(usize, &std::rc::Rc<Tab>)> = window.iter().enumerate().collect();
            for (tab_idx, tab) in ttabs {
                let entry = tab.get_entry(tab_idx).await;
                otabs.push(entry);
            }
            otabs
        } else {
            vec![]
        };

        let domains = if flags.contains(LauncherFlags::DOMAINS) {
            let mut domains = mux.iter_domains();
            domains.sort_by(|a, b| {
                let a_state = a.state();
                let b_state = b.state();
                if a_state != b_state {
                    use std::cmp::Ordering;
                    return if a_state == DomainState::Attached {
                        Ordering::Less
                    } else {
                        Ordering::Greater
                    };
                }
                a.domain_id().cmp(&b.domain_id())
            });
            domains.retain(|dom| dom.spawnable());
            let mut d = vec![];
            for dom in domains.iter() {
                let entry = dom.get_entry(0).await;
                d.push(entry)
            }
            d
        } else {
            vec![]
        };

        Self {
            flags,
            domains,
            tabs,
            cmddefs,
            shortcuts: key_entries,
            entries,
            pane_id,
            domain_id_of_current_tab,
            title: title.to_string(),
            workspaces,
            active_workspace,
        }
    }
}

const ROW_OVERHEAD: usize = 3;

struct LauncherState {
    active_idx: usize,
    max_items: usize,
    top_row: usize,
    entries: Vec<LauncherEntry>,
    filter_term: String,
    filtered_entries: Vec<LauncherEntry>,
    pane_id: PaneId,
    window: ::window::Window,
    filtering: bool,
    flags: LauncherFlags,
}

impl LauncherState {
    fn update_filter(&mut self) {
        if self.filter_term.is_empty() {
            self.filtered_entries = self.entries.clone();
            return;
        }

        self.filtered_entries.clear();

        let matcher = SkimMatcherV2::default();

        struct MatchResult {
            row_idx: usize,
            score: i64,
        }

        let mut scores: Vec<MatchResult> = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(row_idx, entry)| {
                let score = matcher.fuzzy_match(&entry.label, &self.filter_term)?;
                Some(MatchResult { row_idx, score })
            })
            .collect();

        scores.sort_by(|a, b| a.score.cmp(&b.score).reverse());

        for result in scores {
            self.filtered_entries
                .push(self.entries[result.row_idx].clone());
        }

        self.active_idx = 0;
        self.top_row = 0;
    }

    fn build_entries(&mut self, args: LauncherArgs) {
        let config = configuration();
        // Pull in the user defined entries from the launch_menu
        // section of the configuration.
        // if args.flags.contains(LauncherFlags::LAUNCH_MENU_ITEMS) {
        //     for item in &config.launch_menu {
        //         self.entries.push(Entry {
        //             label: match item.label.as_ref() {
        //                 Some(label) => label.to_string(),
        //                 None => match item.args.as_ref() {
        //                     Some(args) => args.join(" "),
        //                     None => "(default shell)".to_string(),
        //                 },
        //             },
        //             action: KeyAssignment::SpawnCommandInNewTab(item.clone()),
        //         });
        //     }
        // }

        for domain in &args.domains {
            self.entries.push(domain.clone());
        }

        // if args.flags.contains(LauncherFlags::WORKSPACES) {
        //     for ws in &args.workspaces {
        //         if *ws != args.active_workspace {
        //             self.entries.push(Entry {
        //                 label: format!("Switch to workspace: `{}`", ws),
        //                 action: KeyAssignment::SwitchToWorkspace {
        //                     name: Some(ws.clone()),
        //                     spawn: None,
        //                 },
        //             });
        //         }
        //     }
        //     self.entries.push(Entry {
        //         label: format!(
        //             "Create new Workspace (current is `{}`)",
        //             args.active_workspace
        //         ),
        //         action: KeyAssignment::SwitchToWorkspace {
        //             name: None,
        //             spawn: None,
        //         },
        //     });
        // }
        for tab in &args.tabs {
            self.entries.push(tab.to_owned());
        }

        for entry in &args.entries {
            self.entries.push(entry.to_owned());
        }

        for cmd in &args.cmddefs {
            self.entries.push(cmd.to_owned());
        }
        for shortcut in &args.shortcuts {
            self.entries.push(shortcut.to_owned());
        }
    }

    fn render(&mut self, term: &mut TermWizTerminal) -> termwiz::Result<()> {
        let size = term.get_screen_size()?;
        let max_width = size.cols.saturating_sub(6);

        let mut changes = vec![
            Change::ClearScreen(ColorAttribute::Default),
            Change::CursorPosition {
                x: Position::Absolute(0),
                y: Position::Absolute(0),
            },
            Change::Text(format!(
                "{}\r\n",
                truncate_right(
                    "Select an item and press Enter=launch  \
                     Esc=cancel  /=filter",
                    max_width
                )
            )),
            Change::AllAttributes(CellAttributes::default()),
        ];

        let max_items = self.max_items;

        for (row_num, (entry_idx, entry)) in self
            .filtered_entries
            .iter()
            .enumerate()
            .skip(self.top_row)
            .enumerate()
        {
            if row_num > max_items {
                break;
            }

            let mut attr = CellAttributes::blank();

            if entry_idx == self.active_idx {
                changes.push(AttributeChange::Reverse(true).into());
                attr.set_reverse(true);
            }

            if row_num < 9 && !self.filtering {
                changes.push(Change::Text(format!(" {}. ", row_num + 1)));
            } else {
                changes.push(Change::Text("    ".to_string()));
            }

            let mut line = crate::tabbar::parse_status_text(&entry.label, attr.clone());
            if line.cells().len() > max_width {
                line.resize(max_width, termwiz::surface::SEQ_ZERO);
            }
            changes.append(&mut line.changes(&attr));
            changes.push(Change::Text(" \r\n".to_string()));

            if entry_idx == self.active_idx {
                changes.push(AttributeChange::Reverse(false).into());
            }
        }

        if self.filtering || !self.filter_term.is_empty() {
            changes.append(&mut vec![
                Change::CursorPosition {
                    x: Position::Absolute(0),
                    y: Position::Absolute(0),
                },
                Change::ClearToEndOfLine(ColorAttribute::Default),
                Change::Text(truncate_right(
                    &format!("Fuzzy matching: {}", self.filter_term),
                    max_width,
                )),
            ]);
        }

        term.render(&changes)
    }

    fn launch(&self, active_idx: usize) {
        let assignment = self.filtered_entries[active_idx].action.clone();
        self.window.notify(TermWindowNotif::PerformAssignment {
            pane_id: self.pane_id,
            assignment,
        });
    }

    fn move_up(&mut self) {
        self.active_idx = self.active_idx.saturating_sub(1);
        if self.active_idx < self.top_row {
            self.top_row = self.active_idx;
        }
    }

    fn move_down(&mut self) {
        self.active_idx = (self.active_idx + 1).min(self.filtered_entries.len() - 1);
        if self.active_idx + self.top_row > self.max_items {
            self.top_row = self.active_idx.saturating_sub(self.max_items);
        }
    }

    fn run_loop(&mut self, term: &mut TermWizTerminal) -> anyhow::Result<()> {
        while let Ok(Some(event)) = term.poll_input(None) {
            match event {
                InputEvent::Key(KeyEvent {
                    key: KeyCode::Char(c),
                    ..
                }) if !self.filtering && c >= '1' && c <= '9' => {
                    self.launch(self.top_row + (c as u32 - '1' as u32) as usize);
                    break;
                }
                InputEvent::Key(KeyEvent {
                    key: KeyCode::Char('j'),
                    ..
                }) if !self.filtering => {
                    self.move_down();
                }
                InputEvent::Key(KeyEvent {
                    key: KeyCode::Char('k'),
                    ..
                }) if !self.filtering => {
                    self.move_up();
                }
                InputEvent::Key(KeyEvent {
                    key: KeyCode::Char('P'),
                    modifiers: Modifiers::CTRL,
                }) => {
                    self.move_up();
                }
                InputEvent::Key(KeyEvent {
                    key: KeyCode::Char('N'),
                    modifiers: Modifiers::CTRL,
                }) => {
                    self.move_down();
                }
                InputEvent::Key(KeyEvent {
                    key: KeyCode::Char('/'),
                    ..
                }) if !self.filtering => {
                    self.filtering = true;
                }
                InputEvent::Key(KeyEvent {
                    key: KeyCode::Backspace,
                    ..
                }) => {
                    if self.filter_term.pop().is_none()
                        && !self.flags.contains(LauncherFlags::FUZZY)
                    {
                        self.filtering = false;
                    }
                    self.update_filter();
                }
                InputEvent::Key(KeyEvent {
                    key: KeyCode::Char('G'),
                    modifiers: Modifiers::CTRL,
                })
                | InputEvent::Key(KeyEvent {
                    key: KeyCode::Escape,
                    ..
                }) => {
                    break;
                }
                InputEvent::Key(KeyEvent {
                    key: KeyCode::Char(c),
                    ..
                }) if self.filtering => {
                    self.filter_term.push(c);
                    self.update_filter();
                }
                InputEvent::Key(KeyEvent {
                    key: KeyCode::UpArrow,
                    ..
                }) => {
                    self.move_up();
                }
                InputEvent::Key(KeyEvent {
                    key: KeyCode::DownArrow,
                    ..
                }) => {
                    self.move_down();
                }
                InputEvent::Mouse(MouseEvent {
                    y, mouse_buttons, ..
                }) if mouse_buttons.contains(MouseButtons::VERT_WHEEL) => {
                    if mouse_buttons.contains(MouseButtons::WHEEL_POSITIVE) {
                        self.top_row = self.top_row.saturating_sub(1);
                    } else {
                        self.top_row += 1;
                        self.top_row = self.top_row.min(
                            self.filtered_entries
                                .len()
                                .saturating_sub(self.max_items)
                                .saturating_sub(1),
                        );
                    }
                    if y > 0 && y as usize <= self.filtered_entries.len() {
                        self.active_idx = self.top_row + y as usize - 1;
                    }
                }
                InputEvent::Mouse(MouseEvent {
                    y, mouse_buttons, ..
                }) => {
                    if y > 0 && y as usize <= self.filtered_entries.len() {
                        self.active_idx = self.top_row + y as usize - 1;

                        if mouse_buttons == MouseButtons::LEFT {
                            self.launch(self.active_idx);
                            break;
                        }
                    }
                    if mouse_buttons != MouseButtons::NONE {
                        // Treat any other mouse button as cancel
                        break;
                    }
                }
                InputEvent::Key(KeyEvent {
                    key: KeyCode::Enter,
                    ..
                }) => {
                    self.launch(self.active_idx);
                    break;
                }
                InputEvent::Resized { rows, .. } => {
                    self.max_items = rows.saturating_sub(ROW_OVERHEAD);
                }
                _ => {}
            }
            self.render(term)?;
        }

        Ok(())
    }
}

pub fn launcher(
    args: LauncherArgs,
    mut term: TermWizTerminal,
    window: ::window::Window,
) -> anyhow::Result<()> {
    let size = term.get_screen_size()?;
    let max_items = size.rows.saturating_sub(ROW_OVERHEAD);
    let mut state = LauncherState {
        active_idx: 0,
        max_items,
        pane_id: args.pane_id,
        top_row: 0,
        entries: vec![],
        filter_term: String::new(),
        filtered_entries: vec![],
        window,
        filtering: args.flags.contains(LauncherFlags::FUZZY),
        flags: args.flags,
    };

    term.set_raw_mode()?;
    term.render(&[Change::Title(args.title.to_string())])?;
    state.build_entries(args);
    state.update_filter();
    state.render(&mut term)?;
    state.run_loop(&mut term)
}
