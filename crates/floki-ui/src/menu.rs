//! Results context menu: item list (pure) plus egui rendering.
//!
//! The item list in spec order is built by [`menu_items`]; the UI inserts
//! separators between the groups (after Reveal, after the copy pair, before
//! Delete). Actions apply to the selected row; multi-select is out of scope.

use crate::model::should_show_runas;

/// One context-menu action, in spec order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Item {
    Open,
    OpenWith,
    Reveal,
    CopyPath,
    CopyName,
    RunAsAdmin,
    Properties,
    Delete,
}

impl Item {
    /// Button label plus shortcut hint (empty when none).
    #[must_use]
    pub fn label(self) -> (&'static str, &'static str) {
        match self {
            Item::Open => ("Open", "Enter"),
            Item::OpenWith => ("Open with...", ""),
            Item::Reveal => ("Open path in Explorer", "Ctrl+Enter"),
            Item::CopyPath => ("Copy full path", "Ctrl+C"),
            Item::CopyName => ("Copy name", ""),
            Item::RunAsAdmin => ("Run as administrator", ""),
            Item::Properties => ("Properties", ""),
            Item::Delete => ("Delete (to Recycle Bin)", "Del"),
        }
    }
}

/// Build the menu for a hit: the full spec list, minus "Run as
/// administrator" unless [`should_show_runas`] says it applies.
#[must_use]
pub fn menu_items(is_dir: bool, full_path: &str) -> Vec<Item> {
    let mut items = vec![
        Item::Open,
        Item::OpenWith,
        Item::Reveal,
        Item::CopyPath,
        Item::CopyName,
    ];
    if should_show_runas(full_path, is_dir) {
        items.push(Item::RunAsAdmin);
    }
    items.push(Item::Properties);
    items.push(Item::Delete);
    items
}

/// Whether a separator belongs above `item` given the previously rendered
/// item (groups: open / copy / admin+properties / delete).
#[must_use]
pub fn separator_before(item: Item, prev: Option<Item>) -> bool {
    matches!(
        (item, prev),
        (Item::CopyPath, _)
            | (Item::Delete, _)
            | (Item::RunAsAdmin | Item::Properties, Some(Item::CopyName))
    )
}

/// Render the menu body; returns the picked action, if any.
pub fn show_menu(ui: &mut egui::Ui, items: &[Item]) -> Option<Item> {
    let mut picked = None;
    let mut prev = None;
    for &item in items {
        if separator_before(item, prev) {
            ui.separator();
        }
        let (label, shortcut) = item.label();
        let button = if shortcut.is_empty() {
            egui::Button::new(label)
        } else {
            egui::Button::new(label).shortcut_text(shortcut)
        };
        if ui.add(button).clicked() {
            picked = Some(item);
        }
        prev = Some(item);
    }
    picked
}

/// The concrete, UI-free effect of picking a menu [`Item`]: paths the shell
/// should act on, text for the clipboard, or a delete-confirmation request.
/// [`dispatch`] maps every item to one of these so the mapping itself is
/// unit-testable without GUI automation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    OpenFile(String),
    OpenWith(String),
    Reveal(String),
    CopyText(String),
    RunAsAdmin(String),
    ShowProperties(String),
    RequestDelete {
        path: String,
        name: String,
        idx: usize,
    },
}

/// Map a picked menu item to its [`Action`] for the selected row
/// (`full_path` rebuilt from the hit, `name` the hit name, `idx` its row).
#[must_use]
pub fn dispatch(item: Item, full_path: &str, name: &str, idx: usize) -> Action {
    match item {
        Item::Open => Action::OpenFile(full_path.to_owned()),
        Item::OpenWith => Action::OpenWith(full_path.to_owned()),
        Item::Reveal => Action::Reveal(full_path.to_owned()),
        Item::CopyPath => Action::CopyText(full_path.to_owned()),
        Item::CopyName => Action::CopyText(name.to_owned()),
        Item::RunAsAdmin => Action::RunAsAdmin(full_path.to_owned()),
        Item::Properties => Action::ShowProperties(full_path.to_owned()),
        Item::Delete => Action::RequestDelete {
            path: full_path.to_owned(),
            name: name.to_owned(),
            idx,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exe_file_gets_the_full_menu() {
        assert_eq!(
            menu_items(false, r"C:\tools\setup.exe"),
            vec![
                Item::Open,
                Item::OpenWith,
                Item::Reveal,
                Item::CopyPath,
                Item::CopyName,
                Item::RunAsAdmin,
                Item::Properties,
                Item::Delete,
            ]
        );
    }

    #[test]
    fn plain_file_and_folder_skip_runas() {
        let file = menu_items(false, r"C:\docs\notes.txt");
        assert!(!file.contains(&Item::RunAsAdmin));
        assert_eq!(file.len(), 7);

        let folder = menu_items(true, r"C:\docs\projects");
        assert!(!folder.contains(&Item::RunAsAdmin));
        assert_eq!(
            folder,
            vec![
                Item::Open,
                Item::OpenWith,
                Item::Reveal,
                Item::CopyPath,
                Item::CopyName,
                Item::Properties,
                Item::Delete,
            ]
        );
    }

    #[test]
    fn separators_group_the_menu() {
        // Open group | copy group | props group | delete.
        assert!(separator_before(Item::CopyPath, Some(Item::Reveal)));
        assert!(!separator_before(Item::CopyName, Some(Item::CopyPath)));
        assert!(separator_before(Item::RunAsAdmin, Some(Item::CopyName)));
        assert!(separator_before(Item::Properties, Some(Item::CopyName)));
        assert!(!separator_before(Item::Properties, Some(Item::RunAsAdmin)));
        assert!(separator_before(Item::Delete, Some(Item::Properties)));
        assert!(!separator_before(Item::Open, None));
    }

    #[test]
    fn labels_carry_the_spec_shortcuts() {
        assert_eq!(Item::Open.label(), ("Open", "Enter"));
        assert_eq!(
            Item::Reveal.label(),
            ("Open path in Explorer", "Ctrl+Enter")
        );
        assert_eq!(Item::CopyPath.label(), ("Copy full path", "Ctrl+C"));
        assert_eq!(Item::Delete.label(), ("Delete (to Recycle Bin)", "Del"));
        assert_eq!(Item::Properties.label().1, "");
    }

    #[test]
    fn dispatch_maps_every_item_for_a_plain_file() {
        let path = r"C:\docs\notes.txt";
        let cases = [
            (Item::Open, Action::OpenFile(path.to_owned())),
            (Item::OpenWith, Action::OpenWith(path.to_owned())),
            (Item::Reveal, Action::Reveal(path.to_owned())),
            (Item::CopyPath, Action::CopyText(path.to_owned())),
            (Item::CopyName, Action::CopyText("notes.txt".to_owned())),
            (Item::Properties, Action::ShowProperties(path.to_owned())),
            (
                Item::Delete,
                Action::RequestDelete {
                    path: path.to_owned(),
                    name: "notes.txt".to_owned(),
                    idx: 3,
                },
            ),
        ];
        for (item, expected) in cases {
            assert_eq!(dispatch(item, path, "notes.txt", 3), expected);
        }
    }

    #[test]
    fn dispatch_covers_folders_and_executables() {
        // Folders never offer RunAsAdmin, but dispatch still maps it if asked.
        let dir = r"C:\docs\projects";
        assert_eq!(
            dispatch(Item::Open, dir, "projects", 0),
            Action::OpenFile(dir.to_owned())
        );
        assert_eq!(
            dispatch(Item::Delete, dir, "projects", 0),
            Action::RequestDelete {
                path: dir.to_owned(),
                name: "projects".to_owned(),
                idx: 0,
            }
        );
        let exe = r"C:\tools\setup.exe";
        assert!(menu_items(false, exe).contains(&Item::RunAsAdmin));
        assert_eq!(
            dispatch(Item::RunAsAdmin, exe, "setup.exe", 1),
            Action::RunAsAdmin(exe.to_owned())
        );
    }
}
