//! The Messages screen: conversations with contacts and groups, the chat, the composer, search,
//! and each group's members.
//!
//! The lists refresh from the node and the database on a timer, so messages that arrive while
//! the screen is open appear without a restart. Times are shown in UTC, since the app has no
//! time zone support.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gpui::{
    App, Context, Entity, ExternalPaths, FontWeight, Hsla, Image, ImageFormat, ListAlignment,
    ListState, PathPromptOptions, Subscription, Window, div, img, list, prelude::*, px,
};
use groovy_core::groups::{GroupDetails, GroupId, GroupMessage};
use groovy_core::membership::Role;
use groovy_core::messages::{Attachment, DeliveryState, HitKind, Message, MessageId, SearchHit};
use groovy_core::{Database, EndpointAddr, EndpointId, NodeHandle, Trust};
use tracing::warn;

use crate::{
    files::format_bytes,
    node::{NodeState, abbreviate_id},
    theme::ActiveTheme,
    ui::{
        avatar::Avatar,
        button::{Button, Variant},
        icon::Icon,
        input::{TextInput, TextInputEvent},
    },
};

/// How often the open chat reads its newest messages.
const CHAT_EVERY: Duration = Duration::from_secs(1);
/// How often the conversation list is read again.
const LIST_EVERY: Duration = Duration::from_secs(3);
/// How many of the newest messages are read on each refresh of the open chat.
const LATEST: usize = 100;
/// How many older messages each "load older" reads.
const PAGE: usize = 50;
/// How many messages a conversation's last message is read from, to count unread ones.
const UNREAD_WINDOW: usize = 100;
/// How many search results are shown.
const HITS: usize = 50;

/// Who a conversation is with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Target {
    Contact(EndpointId),
    Group(GroupId),
}

/// A row of the conversation list.
#[derive(Clone)]
struct Row {
    target: Target,
    label: String,
    last: Option<String>,
    last_at: i64,
    unread: usize,
}

/// A message as the chat shows it, for a direct or a group conversation.
#[derive(Clone, PartialEq)]
struct Line {
    id: MessageId,
    author: EndpointId,
    own: bool,
    timestamp: i64,
    body: String,
    state: Option<DeliveryState>,
    attachments: Vec<Attachment>,
}

impl Line {
    fn from_direct(message: Message, own: &EndpointId) -> Self {
        Self {
            id: message.id,
            author: message.author,
            own: message.author == *own,
            timestamp: message.timestamp,
            body: message.body,
            state: Some(message.state),
            attachments: message.attachments,
        }
    }

    fn from_group(message: GroupMessage, own: &EndpointId) -> Self {
        Self {
            id: message.id,
            author: message.author,
            own: message.author == *own,
            timestamp: message.timestamp,
            body: message.body,
            state: None,
            attachments: Vec::new(),
        }
    }
}

pub struct MessagesView {
    database: Arc<Database>,
    rows: Vec<Row>,
    selected: Option<Target>,
    /// The chat, oldest first.
    lines: Rc<Vec<Line>>,
    /// Whether older messages than the first one shown exist.
    has_older: bool,
    list: ListState,
    search: Entity<TextInput>,
    composer: Entity<TextInput>,
    new_group: Entity<TextInput>,
    join: Entity<TextInput>,
    rename: Entity<TextInput>,
    /// The query the search results are for, and its results.
    query: String,
    hits: Vec<SearchHit>,
    /// The file chosen or dropped for the next message to a contact.
    pending_file: Option<PathBuf>,
    /// The newest message each conversation was read up to, for unread counts.
    read_up_to: HashMap<Target, i64>,
    /// The details of the open group, when its panel is open.
    group: Option<GroupDetails>,
    show_group_info: bool,
    /// A member whose removal is waiting for a second click.
    confirm_remove: Option<EndpointId>,
    /// Whether the open conversation's messages are waiting to be cleared, for a second click.
    confirm_clear: bool,
    /// Attachments the user has asked for, so each shows as downloading once.
    requested: HashSet<(MessageId, usize)>,
    status: Option<(bool, SharedString)>,
    _subscriptions: Vec<Subscription>,
}

use gpui::SharedString;

impl MessagesView {
    pub fn new(database: Arc<Database>, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| TextInput::new("Search messages", cx));
        let composer = cx.new(|cx| TextInput::new("Write a message", cx));
        let new_group = cx.new(|cx| TextInput::new("New group name", cx));
        let join = cx.new(|cx| TextInput::new("Paste a group invitation", cx));
        let rename = cx.new(|cx| TextInput::new("Rename the group", cx));
        let _subscriptions = vec![
            cx.subscribe(&search, |view, _, event: &TextInputEvent, cx| {
                if *event == TextInputEvent::Changed {
                    view.search_changed(cx);
                }
            }),
            cx.subscribe(&composer, |view, _, event: &TextInputEvent, cx| {
                if *event == TextInputEvent::Submitted {
                    view.send(cx);
                }
            }),
            cx.subscribe(&new_group, |view, _, event: &TextInputEvent, cx| {
                if *event == TextInputEvent::Submitted {
                    view.create_group(cx);
                }
            }),
            cx.subscribe(&join, |view, _, event: &TextInputEvent, cx| {
                if *event == TextInputEvent::Submitted {
                    view.join_group(cx);
                }
            }),
        ];
        let mut view = Self {
            database,
            rows: Vec::new(),
            selected: None,
            lines: Rc::new(Vec::new()),
            has_older: false,
            list: ListState::new(0, ListAlignment::Bottom, px(600.)),
            search,
            composer,
            new_group,
            join,
            rename,
            query: String::new(),
            hits: Vec::new(),
            pending_file: None,
            read_up_to: HashMap::new(),
            group: None,
            show_group_info: false,
            confirm_remove: None,
            confirm_clear: false,
            requested: HashSet::new(),
            status: None,
            _subscriptions,
        };
        view.reload_rows(cx);

        // The list of conversations, and the open chat, keep themselves current.
        cx.spawn(async move |this, cx| {
            let mut ticks: u64 = 0;
            loop {
                cx.background_executor().timer(CHAT_EVERY).await;
                ticks += 1;
                let alive = this.update(cx, |view, cx| {
                    view.refresh_chat(cx);
                    if ticks.is_multiple_of(LIST_EVERY.as_secs()) {
                        view.reload_rows(cx);
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        })
        .detach();
        view
    }

    fn handle(cx: &App) -> Option<NodeHandle> {
        cx.try_global::<NodeState>().and_then(|node| node.handle())
    }

    fn own_id(cx: &App) -> Option<EndpointId> {
        Self::handle(cx).map(|handle| handle.endpoint_id())
    }

    fn set_status(
        &mut self,
        error: bool,
        message: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        self.status = Some((error, message.into()));
        cx.notify();
    }

    /// Reads the conversation list in the background: trusted contacts and groups, each with its
    /// last message and how many newer ones have not been read.
    fn reload_rows(&mut self, cx: &mut Context<Self>) {
        let database = self.database.clone();
        let handle = Self::handle(cx);
        let read_up_to = self.read_up_to.clone();
        let own = Self::own_id(cx);
        cx.spawn(async move |this, cx| {
            let mut rows = Vec::new();
            let contacts = {
                let database = database.clone();
                cx.background_spawn(async move { database.contacts() })
                    .await
            };
            match contacts {
                Ok(contacts) => {
                    for contact in contacts.into_iter().filter(|c| c.trust == Trust::Trusted) {
                        let target = Target::Contact(contact.endpoint_id);
                        let database = database.clone();
                        let after = read_up_to.get(&target).copied();
                        let id = contact.endpoint_id;
                        let row = cx
                            .background_spawn(async move {
                                let latest = database.messages_before(&id, None, UNREAD_WINDOW)?;
                                Ok::<_, groovy_core::DbError>(summary(
                                    target,
                                    contact.label().to_owned(),
                                    latest
                                        .into_iter()
                                        .map(|m| (m.timestamp, m.body, m.author))
                                        .collect(),
                                    after,
                                    own,
                                ))
                            })
                            .await;
                        match row {
                            Ok(row) => rows.push(row),
                            Err(error) => warn!(%error, "could not read a conversation"),
                        }
                    }
                }
                Err(error) => warn!(%error, "could not read the contacts"),
            }
            if let Some(handle) = handle {
                match handle.groups().await {
                    Ok(groups) => {
                        for group in groups {
                            let target = Target::Group(group.id);
                            let database = database.clone();
                            let after = read_up_to.get(&target).copied();
                            let id = group.id;
                            let label = group.name.clone();
                            let row = cx
                                .background_spawn(async move {
                                    let latest =
                                        database.group_messages_before(&id, None, UNREAD_WINDOW)?;
                                    Ok::<_, groovy_core::DbError>(summary(
                                        target,
                                        label,
                                        latest
                                            .into_iter()
                                            .map(|m| (m.timestamp, m.body, m.author))
                                            .collect(),
                                        after,
                                        own,
                                    ))
                                })
                                .await;
                            match row {
                                Ok(row) => rows.push(row),
                                Err(error) => warn!(%error, "could not read a group"),
                            }
                        }
                    }
                    Err(error) => warn!(%error, "could not read the groups"),
                }
            }
            rows.sort_by(|a, b| b.last_at.cmp(&a.last_at).then(a.label.cmp(&b.label)));
            this.update(cx, |view, cx| {
                for row in &rows {
                    view.read_up_to.entry(row.target).or_insert(row.last_at);
                }
                view.rows = rows;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Opens the conversation a notification was about.
    pub fn open_conversation(
        &mut self,
        destination: crate::notifications::Destination,
        cx: &mut Context<Self>,
    ) {
        let target = match destination {
            crate::notifications::Destination::Contact(id) => Target::Contact(id),
            crate::notifications::Destination::Group(id) => Target::Group(id),
            _ => return,
        };
        self.select(target, cx);
    }

    /// Opens a conversation: its newest messages are read, and it counts as read to the newest.
    fn select(&mut self, target: Target, cx: &mut Context<Self>) {
        self.selected = Some(target);
        self.lines = Rc::new(Vec::new());
        self.has_older = false;
        self.list.reset(0);
        self.group = None;
        self.show_group_info = false;
        self.confirm_remove = None;
        self.confirm_clear = false;
        self.pending_file = None;
        self.refresh_chat(cx);
        if let Target::Group(group) = target {
            self.load_group(group, cx);
        }
        cx.notify();
    }

    /// Reads the open chat's newest messages, and adds any that are new.
    fn refresh_chat(&mut self, cx: &mut Context<Self>) {
        let Some(target) = self.selected else {
            return;
        };
        let database = self.database.clone();
        let own = Self::own_id(cx);
        let Some(own) = own else {
            return;
        };
        let read = cx.background_spawn(async move {
            match target {
                Target::Contact(id) => {
                    database.messages_before(&id, None, LATEST).map(|messages| {
                        messages
                            .into_iter()
                            .map(|m| Line::from_direct(m, &own))
                            .collect::<Vec<_>>()
                    })
                }
                Target::Group(id) => {
                    database
                        .group_messages_before(&id, None, LATEST)
                        .map(|messages| {
                            messages
                                .into_iter()
                                .map(|m| Line::from_group(m, &own))
                                .collect::<Vec<_>>()
                        })
                }
            }
        });
        cx.spawn(async move |this, cx| {
            let latest = match read.await {
                Ok(latest) => latest,
                Err(error) => {
                    warn!(%error, "could not read the chat");
                    return;
                }
            };
            this.update(cx, |view, cx| {
                if view.selected != Some(target) {
                    return;
                }
                view.merge_latest(latest);
                if let Some(newest) = view.lines.last() {
                    view.read_up_to.insert(target, newest.timestamp);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Puts a fresh page of the newest messages into the chat. Messages already shown keep their
    /// place; new ones are added in order, and the list is rebuilt only when something changed.
    fn merge_latest(&mut self, latest: Vec<Line>) {
        let mut lines: Vec<Line> = self.lines.iter().cloned().collect();
        let known: HashMap<MessageId, usize> =
            lines.iter().enumerate().map(|(i, l)| (l.id, i)).collect();
        let mut changed = false;
        for line in latest {
            match known.get(&line.id) {
                Some(&index) => {
                    if lines[index] != line {
                        lines[index] = line;
                        changed = true;
                    }
                }
                None => {
                    lines.push(line);
                    changed = true;
                }
            }
        }
        if changed {
            lines.sort_by(|a, b| a.timestamp.cmp(&b.timestamp).then(a.id.cmp(&b.id)));
            self.list.reset(lines.len() + usize::from(self.has_older));
            self.lines = Rc::new(lines);
        }
        if self.lines.is_empty() {
            self.has_older = false;
        }
    }

    /// Reads the messages before the oldest one shown, and puts them at the top.
    fn load_older(&mut self, cx: &mut Context<Self>) {
        let Some(target) = self.selected else {
            return;
        };
        let Some(first) = self.lines.first() else {
            return;
        };
        let cursor = (first.timestamp, first.id);
        let database = self.database.clone();
        let Some(own) = Self::own_id(cx) else {
            return;
        };
        let read =
            cx.background_spawn(async move {
                match target {
                    Target::Contact(id) => {
                        database
                            .messages_before(&id, Some(cursor), PAGE)
                            .map(|messages| {
                                messages
                                    .into_iter()
                                    .map(|m| Line::from_direct(m, &own))
                                    .collect::<Vec<_>>()
                            })
                    }
                    Target::Group(id) => database
                        .group_messages_before(&id, Some(cursor), PAGE)
                        .map(|messages| {
                            messages
                                .into_iter()
                                .map(|m| Line::from_group(m, &own))
                                .collect::<Vec<_>>()
                        }),
                }
            });
        cx.spawn(async move |this, cx| {
            let older = match read.await {
                Ok(older) => older,
                Err(error) => {
                    warn!(%error, "could not read older messages");
                    return;
                }
            };
            this.update(cx, |view, cx| {
                if view.selected != Some(target) {
                    return;
                }
                view.has_older = older.len() == PAGE;
                let mut lines: Vec<Line> = older;
                lines.extend(view.lines.iter().cloned());
                view.lines = Rc::new(lines);
                view.list
                    .reset(view.lines.len() + usize::from(view.has_older));
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Reads the open group's details, for its panel.
    fn load_group(&mut self, group: GroupId, cx: &mut Context<Self>) {
        let Some(handle) = Self::handle(cx) else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let details = handle.group_details(group).await;
            this.update(cx, |view, cx| {
                match details {
                    Ok(details) => {
                        view.rename
                            .update(cx, |input, cx| input.set_text(details.name.clone(), cx));
                        view.group = Some(details);
                    }
                    Err(error) => {
                        view.set_status(true, format!("The group could not be read: {error}"), cx)
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn send(&mut self, cx: &mut Context<Self>) {
        let Some(target) = self.selected else {
            return;
        };
        let text = self.composer.read(cx).text().to_owned();
        let file = self.pending_file.clone();
        if text.trim().is_empty() && file.is_none() {
            return;
        }
        let Some(handle) = Self::handle(cx) else {
            self.set_status(true, "The node is not running yet.", cx);
            return;
        };
        if file.is_some() && matches!(target, Target::Group(_)) {
            self.set_status(
                true,
                "Files can't be sent to a group yet. Send them to a contact.",
                cx,
            );
            return;
        }
        self.composer
            .update(cx, |input, cx| input.set_text(String::new(), cx));
        self.pending_file = None;
        self.status = None;
        cx.spawn(async move |this, cx| {
            let outcome = match (target, file) {
                (Target::Contact(id), Some(path)) => handle
                    .send_attachment(EndpointAddr::from(id), text, path)
                    .await
                    .map(|_| ()),
                (Target::Contact(id), None) => handle
                    .send_message(EndpointAddr::from(id), text, None)
                    .await
                    .map(|_| ()),
                (Target::Group(group), _) => handle
                    .send_group_message(group, text, None)
                    .await
                    .map(|_| ()),
            };
            this.update(cx, |view, cx| {
                if let Err(error) = outcome {
                    view.set_status(true, format!("The message could not be sent: {error}"), cx);
                }
                view.refresh_chat(cx);
            })
            .ok();
        })
        .detach();
    }

    /// Chooses a file to send with the next message to a contact.
    fn choose_file(&mut self, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Choose a file to send".into()),
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = picked.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            this.update(cx, |view, cx| {
                view.pending_file = Some(path);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn drop_files(&mut self, paths: &ExternalPaths, cx: &mut Context<Self>) {
        if let Some(path) = paths.0.first().cloned() {
            self.pending_file = Some(path);
            cx.notify();
        }
    }

    /// Asks the node for an attachment, which then arrives as a transfer.
    fn download(&mut self, message: MessageId, index: usize, name: String, cx: &mut Context<Self>) {
        let Some(handle) = Self::handle(cx) else {
            return;
        };
        self.requested.insert((message, index));
        cx.spawn(async move |this, cx| {
            let outcome = handle.download_attachment(message, index).await;
            this.update(cx, |view, cx| {
                match outcome {
                    Ok(_) => view.set_status(
                        false,
                        format!("Downloading {name}. Progress is in Files."),
                        cx,
                    ),
                    Err(error) => {
                        view.requested.remove(&(message, index));
                        view.set_status(
                            true,
                            format!("{name} could not be downloaded: {error}"),
                            cx,
                        );
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn search_changed(&mut self, cx: &mut Context<Self>) {
        let query = self.search.read(cx).text().trim().to_owned();
        self.query = query.clone();
        if query.is_empty() {
            self.hits.clear();
            cx.notify();
            return;
        }
        let Some(handle) = Self::handle(cx) else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let found = handle.search_messages(query.clone(), HITS).await;
            this.update(cx, |view, cx| {
                if view.query == query {
                    match found {
                        Ok(hits) => view.hits = hits,
                        Err(error) => {
                            view.set_status(true, format!("The search failed: {error}"), cx)
                        }
                    }
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn create_group(&mut self, cx: &mut Context<Self>) {
        let name = self.new_group.read(cx).text().trim().to_owned();
        if name.starts_with("groovygroup1") {
            // An invitation pasted here is joined, not made into a group named after it.
            self.join.update(cx, |input, cx| input.set_text(name, cx));
            self.new_group
                .update(cx, |input, cx| input.set_text(String::new(), cx));
            self.join_group(cx);
            return;
        }
        if name.is_empty() {
            self.set_status(
                true,
                "Type a name for the group first, or paste an invitation to join one.",
                cx,
            );
            return;
        }
        let Some(handle) = Self::handle(cx) else {
            self.set_status(true, "The node is not running yet.", cx);
            return;
        };
        self.new_group
            .update(cx, |input, cx| input.set_text(String::new(), cx));
        cx.spawn(async move |this, cx| {
            let outcome = handle.create_group(name).await;
            this.update(cx, |view, cx| match outcome {
                Ok(group) => {
                    view.reload_rows(cx);
                    view.select(Target::Group(group), cx);
                    view.set_status(
                        false,
                        "Group made. Use Invite in its header to bring people in.",
                        cx,
                    );
                }
                Err(error) => {
                    view.set_status(true, format!("The group could not be made: {error}"), cx)
                }
            })
            .ok();
        })
        .detach();
    }

    /// Runs a change to a group on the node, then reads the group again.
    fn group_op<Fut>(
        &mut self,
        group: GroupId,
        cx: &mut Context<Self>,
        op: impl FnOnce(NodeHandle) -> Fut + 'static,
    ) where
        Fut: std::future::Future<Output = Result<(), groovy_core::NodeError>> + 'static,
    {
        let Some(handle) = Self::handle(cx) else {
            return;
        };
        let future = op(handle);
        cx.spawn(async move |this, cx| {
            let outcome = future.await;
            this.update(cx, |view, cx| match outcome {
                Ok(()) => {
                    view.set_status(false, "Saved.", cx);
                    view.load_group(group, cx);
                    view.reload_rows(cx);
                }
                Err(error) => {
                    view.set_status(true, format!("The change was not saved: {error}"), cx)
                }
            })
            .ok();
        })
        .detach();
    }

    /// Fills in the join field with an invitation that was opened. Joining waits for the user.
    pub fn prefill_join(&mut self, text: String, cx: &mut Context<Self>) {
        self.join.update(cx, |input, cx| input.set_text(text, cx));
        self.set_status(false, "Check the invitation, then press Join.", cx);
    }

    /// Joins the group whose invitation is in the join field.
    fn join_group(&mut self, cx: &mut Context<Self>) {
        let ticket = self.join.read(cx).text().trim().to_owned();
        if ticket.is_empty() {
            self.set_status(true, "Paste an invitation first.", cx);
            return;
        }
        let Some(handle) = Self::handle(cx) else {
            self.set_status(true, "The node is not running yet.", cx);
            return;
        };
        self.join
            .update(cx, |input, cx| input.set_text(String::new(), cx));
        cx.spawn(async move |this, cx| {
            let outcome = handle.join_group(ticket).await;
            this.update(cx, |view, cx| match outcome {
                Ok(group) => {
                    view.reload_rows(cx);
                    view.select(Target::Group(group), cx);
                    view.set_status(
                        false,
                        "Joined the group. Its messages arrive as members answer.",
                        cx,
                    );
                }
                Err(error) => view.set_status(
                    true,
                    format!("That invitation could not be used: {error}"),
                    cx,
                ),
            })
            .ok();
        })
        .detach();
    }

    fn copy_invite(&mut self, group: GroupId, cx: &mut Context<Self>) {
        let Some(handle) = Self::handle(cx) else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let ticket = handle.group_ticket(group).await;
            this.update(cx, |view, cx| match ticket {
                Ok(ticket) => {
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(ticket));
                    view.set_status(
                        false,
                        "Invitation copied. Send it to someone you have paired with; they paste it under Join a group.",
                        cx,
                    );
                }
                Err(error) => {
                    view.set_status(true, format!("No invitation could be made: {error}"), cx)
                }
            })
            .ok();
        })
        .detach();
    }

    fn clear_open_conversation(&mut self, cx: &mut Context<Self>) {
        self.confirm_clear = false;
        let Some(target) = self.selected else {
            return;
        };
        let Some(handle) = Self::handle(cx) else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let outcome = match target {
                Target::Contact(id) => handle.clear_conversation(id).await,
                Target::Group(id) => handle.clear_group_messages(id).await,
            };
            this.update(cx, |view, cx| {
                match outcome {
                    Ok(_) => {
                        view.lines = Rc::new(Vec::new());
                        view.has_older = false;
                        view.list.reset(0);
                        view.set_status(
                            false,
                            "Cleared on this device. The other people keep theirs.",
                            cx,
                        );
                        view.reload_rows(cx);
                    }
                    Err(error) => view.set_status(true, format!("Could not clear: {error}"), cx),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn label_of(&self, id: &EndpointId) -> String {
        self.rows
            .iter()
            .find(|row| row.target == Target::Contact(*id))
            .map(|row| row.label.clone())
            .unwrap_or_else(|| abbreviate_id(&id.to_string()))
    }

    fn presence(cx: &App, id: &EndpointId) -> bool {
        cx.try_global::<NodeState>()
            .and_then(|node| node.route(id))
            .is_some()
    }
}

/// The conversation row for one conversation, from its latest messages: `(timestamp, body,
/// author)`, oldest first.
fn summary(
    target: Target,
    label: String,
    latest: Vec<(i64, String, EndpointId)>,
    after: Option<i64>,
    own: Option<EndpointId>,
) -> Row {
    let (last_at, last) = latest
        .last()
        .map(|(at, body, author)| {
            let who = if own == Some(*author) { "You: " } else { "" };
            (*at, Some(format!("{who}{}", preview(body))))
        })
        .unwrap_or((0, None));
    // Messages before the conversation was first seen count as read: only later ones are unread.
    let unread = latest
        .iter()
        .filter(|(at, _, author)| Some(*author) != own && after.is_some_and(|read| *at > read))
        .count();
    Row {
        target,
        label,
        last,
        last_at,
        unread,
    }
}

/// The start of a message, for a list row.
fn preview(body: &str) -> String {
    let single: String = body.split_whitespace().collect::<Vec<_>>().join(" ");
    if single.chars().count() > 60 {
        format!("{}…", single.chars().take(60).collect::<String>())
    } else if single.is_empty() {
        "Attachment".to_owned()
    } else {
        single
    }
}

/// The UTC day a millisecond timestamp falls on, counted from the Unix epoch.
fn day_of(timestamp: i64) -> i64 {
    timestamp.div_euclid(86_400_000)
}

/// `YYYY-MM-DD` for a day counted from the Unix epoch.
fn date_of(day: i64) -> String {
    let z = day + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

/// `HH:MM` in UTC.
fn clock_of(timestamp: i64) -> String {
    let minutes = timestamp.div_euclid(60_000).rem_euclid(24 * 60);
    format!("{:02}:{:02}", minutes / 60, minutes % 60)
}

fn state_label(state: DeliveryState) -> (&'static str, bool) {
    match state {
        DeliveryState::Sending => ("Sending", false),
        DeliveryState::Sent => ("Sent", false),
        DeliveryState::Delivered => ("Delivered", false),
        DeliveryState::Failed => ("Not delivered", true),
    }
}

fn role_label(role: Role) -> &'static str {
    match role {
        Role::Admin => "Admin",
        Role::Member => "Member",
    }
}

/// Colours the chat rows use, copied out of the theme so the row builder does not need it.
#[derive(Clone, Copy)]
struct Palette {
    text: Hsla,
    muted: Hsla,
    border: Hsla,
    accent: Hsla,
    on_accent: Hsla,
    danger: Hsla,
    surface: Hsla,
}

impl Render for MessagesView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let palette = Palette {
            text: theme.text,
            muted: theme.text_muted,
            border: theme.border,
            accent: theme.accent,
            on_accent: theme.on_accent,
            danger: theme.danger,
            surface: theme.surface,
        };
        let view = cx.entity().clone();
        let own = Self::own_id(cx);

        // The left column: a new group, the search, and either the search results or the rows.
        let searching = !self.query.is_empty();
        let list_body = if searching {
            self.hits_view(&view, palette).into_any_element()
        } else {
            self.rows_view(&view, palette, cx).into_any_element()
        };
        let left = div()
            .flex()
            .flex_col()
            .gap_2()
            .w(px(300.))
            .flex_none()
            .min_h_0()
            .h_full()
            .p_3()
            .border_r_1()
            .border_color(theme.border)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .child(div().flex_1().min_w_0().child(self.new_group.clone()))
                    .child(
                        Button::new("messages-new-group", "Make")
                            .variant(Variant::Secondary)
                            .on_click({
                                let view = view.clone();
                                move |_, cx| view.update(cx, |view, cx| view.create_group(cx))
                            }),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .child(div().flex_1().min_w_0().child(self.join.clone()))
                    .child(
                        Button::new("messages-join", "Join")
                            .variant(Variant::Secondary)
                            .on_click({
                                let view = view.clone();
                                move |_, cx| view.update(cx, |view, cx| view.join_group(cx))
                            }),
                    ),
            )
            .child(self.search.clone())
            .child(
                div()
                    .id("messages-rows")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(list_body),
            );

        // The right side: the open conversation, its chat, and the composer.
        let right = match self.selected {
            None => div()
                .flex()
                .flex_1()
                .items_center()
                .justify_center()
                .text_sm()
                .text_color(palette.muted)
                .child("Choose a conversation, or make a group.")
                .into_any_element(),
            Some(target) => self
                .conversation_view(target, &view, palette, own, cx)
                .into_any_element(),
        };

        div()
            .flex()
            .flex_row()
            .size_full()
            .min_h_0()
            .child(left)
            .child(right)
    }
}

impl MessagesView {
    /// The conversation rows, most recent first. Each opens its conversation.
    fn rows_view(&self, view: &Entity<Self>, palette: Palette, cx: &App) -> impl IntoElement {
        let rows: Vec<_> = self
            .rows
            .iter()
            .enumerate()
            .map(|(index, row)| {
                let target = row.target;
                let selected = self.selected == Some(target);
                let online = match target {
                    Target::Contact(id) => Self::presence(cx, &id),
                    Target::Group(_) => false,
                };
                let open = view.clone();
                let badge = (row.unread > 0).then(|| {
                    div()
                        .flex_none()
                        .px_1p5()
                        .rounded_full()
                        .bg(palette.accent)
                        .text_color(palette.on_accent)
                        .text_xs()
                        .child(if row.unread > 99 {
                            "99+".to_owned()
                        } else {
                            row.unread.to_string()
                        })
                });
                div()
                    .id(("conversation", index))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1p5()
                    .rounded_md()
                    .cursor_pointer()
                    .when(selected, |d| d.bg(palette.surface))
                    .on_click(move |_, _, cx| open.update(cx, |view, cx| view.select(target, cx)))
                    .child(Avatar::new(row.label.clone()).size(px(28.)))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(div().text_sm().child(row.label.clone()))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(palette.muted)
                                    .overflow_hidden()
                                    .child(
                                        row.last
                                            .clone()
                                            .unwrap_or_else(|| "No messages yet".to_owned()),
                                    ),
                            ),
                    )
                    .when(online, |d| {
                        d.child(div().size(px(8.)).rounded_full().bg(palette.accent))
                    })
                    .children(badge)
            })
            .collect();
        if rows.is_empty() {
            div()
                .text_sm()
                .text_color(palette.muted)
                .child("No trusted contacts yet. Pair with someone, then message them here.")
                .into_any_element()
        } else {
            div()
                .flex()
                .flex_col()
                .gap_1()
                .children(rows)
                .into_any_element()
        }
    }

    /// The search results. A result opens its conversation.
    fn hits_view(&self, view: &Entity<Self>, palette: Palette) -> impl IntoElement {
        if self.hits.is_empty() {
            return div()
                .text_sm()
                .text_color(palette.muted)
                .child("No messages match.")
                .into_any_element();
        }
        let rows: Vec<_> = self
            .hits
            .iter()
            .enumerate()
            .map(|(index, hit)| {
                let target = match hit.kind {
                    HitKind::Direct(id) => Target::Contact(id),
                    HitKind::Group(id) => Target::Group(id),
                };
                let label = self
                    .rows
                    .iter()
                    .find(|row| row.target == target)
                    .map(|row| row.label.clone())
                    .unwrap_or_default();
                let open = view.clone();
                div()
                    .id(("hit", index))
                    .flex()
                    .flex_col()
                    .px_2()
                    .py_1p5()
                    .rounded_md()
                    .cursor_pointer()
                    .on_click(move |_, _, cx| {
                        open.update(cx, |view, cx| {
                            view.search
                                .update(cx, |input, cx| input.set_text(String::new(), cx));
                            view.query.clear();
                            view.hits.clear();
                            view.select(target, cx);
                        })
                    })
                    .child(div().text_xs().text_color(palette.muted).child(label))
                    .child(div().text_sm().child(preview(&hit.body)))
            })
            .collect();
        div()
            .flex()
            .flex_col()
            .gap_1()
            .children(rows)
            .into_any_element()
    }

    /// The open conversation: its header, chat, and composer. A group's panel sits beside it.
    fn conversation_view(
        &self,
        target: Target,
        view: &Entity<Self>,
        palette: Palette,
        own: Option<EndpointId>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let label = self
            .rows
            .iter()
            .find(|row| row.target == target)
            .map(|row| row.label.clone())
            .unwrap_or_default();
        let presence = match target {
            Target::Contact(id) => {
                if Self::presence(cx, &id) {
                    "Online"
                } else {
                    "Offline"
                }
            }
            Target::Group(_) => "Group",
        };

        let header_view = view.clone();
        let clear_view = view.clone();
        let mut header_buttons = div().flex().flex_row().gap_2();
        let destination = match target {
            Target::Contact(id) => crate::notifications::Destination::Contact(id),
            Target::Group(id) => crate::notifications::Destination::Group(id),
        };
        let muted = cx.try_global::<crate::data::AppData>().is_some_and(|data| {
            crate::notifications::is_muted(&data.config.notifications, destination)
        });
        header_buttons = header_buttons.child(
            Button::new("messages-mute", if muted { "Unmute" } else { "Mute" })
                .variant(Variant::Ghost)
                .tooltip("Turn this conversation's notifications on or off")
                .on_click(move |_, cx| crate::notifications::set_muted(cx, destination, muted)),
        );
        if let Target::Group(group) = target {
            let invite_view = view.clone();
            header_buttons = header_buttons.child(
                Button::new("messages-invite", "Invite")
                    .variant(Variant::Secondary)
                    .tooltip("Make an invitation to this group")
                    .on_click(move |_, cx| {
                        invite_view.update(cx, |view, cx| view.copy_invite(group, cx))
                    }),
            );
            header_buttons = header_buttons.child(
                Button::new(
                    "messages-group-info",
                    if self.show_group_info {
                        "Hide members"
                    } else {
                        "Members"
                    },
                )
                .variant(Variant::Secondary)
                .on_click(move |_, cx| {
                    header_view.update(cx, |view, cx| {
                        view.show_group_info = !view.show_group_info;
                        if view.show_group_info {
                            view.load_group(group, cx);
                        }
                        cx.notify();
                    })
                }),
            );
        }
        let clear_label = if self.confirm_clear {
            "Confirm clear"
        } else {
            "Clear"
        };
        header_buttons = header_buttons.child(
            Button::new("messages-clear", clear_label)
                .variant(if self.confirm_clear {
                    Variant::Danger
                } else {
                    Variant::Ghost
                })
                .on_click(move |_, cx| {
                    clear_view.update(cx, |view, cx| {
                        if view.confirm_clear {
                            view.clear_open_conversation(cx);
                        } else {
                            view.confirm_clear = true;
                            cx.notify();
                        }
                    })
                }),
        );

        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .px_4()
            .py_2()
            .border_b_1()
            .border_color(palette.border)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .child(div().font_weight(FontWeight::SEMIBOLD).child(label))
                    .child(div().text_xs().text_color(palette.muted).child(presence)),
            )
            .child(header_buttons);

        let chat = self.chat_view(view, palette);
        let composer = self.composer_view(view, palette, target);
        let status = self.status.as_ref().map(|(error, message)| {
            div()
                .text_xs()
                .text_color(if *error {
                    palette.danger
                } else {
                    palette.muted
                })
                .child(message.clone())
        });

        let panel = if self.show_group_info {
            if let Target::Group(group) = target {
                Some(self.group_panel(group, view, palette, own))
            } else {
                None
            }
        } else {
            None
        };

        div()
            .flex()
            .flex_row()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .child(header)
                    .child(chat)
                    .children(status)
                    .child(composer),
            )
            .children(panel)
    }

    /// The chat: a virtual list of messages, bottom-aligned, that accepts dropped files.
    fn chat_view(&self, view: &Entity<Self>, palette: Palette) -> impl IntoElement {
        let lines = self.lines.clone();
        let has_older = self.has_older;
        let row_view = view.clone();
        let drop_view = view.clone();
        let names: HashMap<EndpointId, String> = self
            .rows
            .iter()
            .filter_map(|row| match row.target {
                Target::Contact(id) => Some((id, row.label.clone())),
                Target::Group(_) => None,
            })
            .collect();
        let is_group = matches!(self.selected, Some(Target::Group(_)));
        let requested = self.requested.clone();
        let chat = list(self.list.clone(), move |index, _window, _cx| {
            if has_older && index == 0 {
                let older_view = row_view.clone();
                return div()
                    .flex()
                    .justify_center()
                    .py_2()
                    .child(
                        Button::new("messages-older", "Load older messages")
                            .variant(Variant::Ghost)
                            .on_click(move |_, cx| {
                                older_view.update(cx, |view, cx| view.load_older(cx))
                            }),
                    )
                    .into_any_element();
            }
            let at = index - usize::from(has_older);
            let Some(line) = lines.get(at) else {
                return div().into_any_element();
            };
            let previous = at.checked_sub(1).and_then(|i| lines.get(i));
            let new_day = previous.is_none_or(|p| day_of(p.timestamp) != day_of(line.timestamp));
            let new_author = previous.is_none_or(|p| p.author != line.author);
            let author_name = if line.own {
                "You".to_owned()
            } else {
                names
                    .get(&line.author)
                    .cloned()
                    .unwrap_or_else(|| abbreviate_id(&line.author.to_string()))
            };
            let mut column = div().flex().flex_col().px_4().py_0p5();
            if new_day {
                column = column.child(
                    div()
                        .flex()
                        .justify_center()
                        .py_2()
                        .text_xs()
                        .text_color(palette.muted)
                        .child(date_of(day_of(line.timestamp))),
                );
            }
            let show_author = is_group && new_author && !line.own;
            let mut bubble = div()
                .flex()
                .flex_col()
                .max_w(px(520.))
                .px_3()
                .py_2()
                .rounded_lg()
                .bg(if line.own {
                    palette.accent
                } else {
                    palette.surface
                })
                .text_color(if line.own {
                    palette.on_accent
                } else {
                    palette.text
                });
            if show_author {
                bubble = bubble.child(
                    div()
                        .text_xs()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(author_name.clone()),
                );
            }
            if !line.body.is_empty() {
                bubble = bubble.child(div().text_sm().child(line.body.clone()));
            }
            for (index, attachment) in line.attachments.iter().enumerate() {
                bubble = bubble.child(attachment_row(
                    line.id, index, attachment, line, &row_view, palette, &requested,
                ));
            }
            let mut meta = div()
                .flex()
                .flex_row()
                .gap_2()
                .text_xs()
                .text_color(palette.muted);
            meta = meta.child(clock_of(line.timestamp));
            if let Some(state) = line.state {
                let (text, failed) = state_label(state);
                meta = meta.child(
                    div()
                        .text_color(if failed {
                            palette.danger
                        } else {
                            palette.muted
                        })
                        .child(text),
                );
            }
            let row = div()
                .flex()
                .flex_col()
                .gap_0p5()
                .mt(if new_author { px(6.) } else { px(0.) })
                .w_full()
                .when(line.own, |d| d.items_end())
                .when(!line.own, |d| d.items_start())
                .child(bubble)
                .child(meta);
            column = column.child(row);
            column.into_any_element()
        })
        .flex_1()
        .min_h_0();

        div()
            .id("messages-chat")
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .child(chat)
            .on_drop::<ExternalPaths>(move |paths, _, cx| {
                drop_view.update(cx, |view, cx| view.drop_files(paths, cx))
            })
    }

    /// The composer: an attachment chooser, the text, and send.
    fn composer_view(
        &self,
        view: &Entity<Self>,
        palette: Palette,
        target: Target,
    ) -> impl IntoElement {
        let attach_view = view.clone();
        let send_view = view.clone();
        let clear_file_view = view.clone();
        let pending = self.pending_file.as_ref().map(|path| {
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .text_xs()
                .text_color(palette.muted)
                .child(format!("Attaching {name}"))
                .child(
                    Button::new("messages-drop-file", "Remove")
                        .variant(Variant::Ghost)
                        .on_click(move |_, cx| {
                            clear_file_view.update(cx, |view, cx| {
                                view.pending_file = None;
                                cx.notify();
                            })
                        }),
                )
        });
        let attach_button = if matches!(target, Target::Contact(_)) {
            Button::icon_only("messages-attach", Icon::File)
                .variant(Variant::Ghost)
                .tooltip("Attach a file")
                .on_click(move |_, cx| attach_view.update(cx, |view, cx| view.choose_file(cx)))
                .into_any_element()
        } else {
            div().into_any_element()
        };
        div()
            .flex()
            .flex_col()
            .gap_1()
            .p_3()
            .border_t_1()
            .border_color(palette.border)
            .children(pending)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(attach_button)
                    .child(div().flex_1().min_w_0().child(self.composer.clone()))
                    .child(
                        Button::new("messages-send", "Send")
                            .variant(Variant::Primary)
                            .on_click(move |_, cx| send_view.update(cx, |view, cx| view.send(cx))),
                    ),
            )
    }

    /// The group's panel: its members and their roles, and what an admin can change.
    fn group_panel(
        &self,
        group: GroupId,
        view: &Entity<Self>,
        palette: Palette,
        own: Option<EndpointId>,
    ) -> impl IntoElement {
        let Some(details) = self.group.as_ref() else {
            return div()
                .w(px(280.))
                .flex_none()
                .p_3()
                .text_sm()
                .text_color(palette.muted)
                .child("Reading the group...")
                .into_any_element();
        };
        let admin = details.you_are_admin;
        let rows: Vec<_> = details
            .members
            .iter()
            .enumerate()
            .map(|(index, (member, role))| {
                let is_you = own == Some(*member);
                let name = self.label_of(member);
                let member = *member;
                let role = *role;
                let make_view = view.clone();
                let remove_view = view.clone();
                let confirming = self.confirm_remove == Some(member);
                let mut actions = div().flex().flex_row().gap_1();
                if admin && !is_you {
                    let (next, label) = match role {
                        Role::Admin => (Some(Role::Member), "Make member"),
                        Role::Member => (Some(Role::Admin), "Make admin"),
                    };
                    actions = actions.child(
                        Button::new(("member-role", index), label)
                            .variant(Variant::Ghost)
                            .on_click(move |_, cx| {
                                make_view.update(cx, |view, cx| {
                                    view.group_op(group, cx, move |handle| async move {
                                        handle.set_group_role(group, member, next).await
                                    })
                                })
                            }),
                    );
                    actions = actions.child(
                        Button::new(
                            ("member-remove", index),
                            if confirming { "Confirm" } else { "Remove" },
                        )
                        .variant(if confirming {
                            Variant::Danger
                        } else {
                            Variant::Ghost
                        })
                        .on_click(move |_, cx| {
                            remove_view.update(cx, |view, cx| {
                                if view.confirm_remove == Some(member) {
                                    view.confirm_remove = None;
                                    view.group_op(group, cx, move |handle| async move {
                                        handle.remove_group_member(group, member).await
                                    });
                                } else {
                                    view.confirm_remove = Some(member);
                                    cx.notify();
                                }
                            })
                        }),
                    );
                }
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .py_1()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .min_w_0()
                            .child(div().text_sm().child(if is_you {
                                format!("{name} (you)")
                            } else {
                                name
                            }))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(palette.muted)
                                    .child(role_label(role)),
                            ),
                    )
                    .child(actions)
            })
            .collect();

        let invite_view = view.clone();
        let rename_view = view.clone();
        let rename_input = self.rename.clone();
        let mut panel = div()
            .flex()
            .flex_col()
            .gap_2()
            .w(px(300.))
            .flex_none()
            .min_h_0()
            .p_3()
            .border_l_1()
            .border_color(palette.border)
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(format!("{} members", details.members.len())),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(palette.muted)
                    .child(format!("Key epoch {}", details.epoch)),
            )
            .child(
                Button::new("group-invite", "Copy an invitation")
                    .variant(Variant::Secondary)
                    .on_click(move |_, cx| {
                        invite_view.update(cx, |view, cx| view.copy_invite(group, cx))
                    }),
            )
            .child(div().flex().flex_col().gap_1().children(rows));
        if admin {
            panel = panel.child(rename_input).child(
                Button::new("group-rename", "Rename")
                    .variant(Variant::Secondary)
                    .on_click(move |_, cx| {
                        rename_view.update(cx, |view, cx| {
                            let name = view.rename.read(cx).text().trim().to_owned();
                            view.group_op(group, cx, move |handle| async move {
                                handle.rename_group(group, name).await
                            });
                        })
                    }),
            );
        }
        panel.into_any_element()
    }
}

/// One attachment of a message: its name and size, a download button, and its preview if it has
/// one.
fn attachment_row(
    message: MessageId,
    index: usize,
    attachment: &Attachment,
    line: &Line,
    view: &Entity<MessagesView>,
    palette: Palette,
    requested: &HashSet<(MessageId, usize)>,
) -> impl IntoElement {
    let preview = attachment.preview.as_ref().map(|bytes| {
        let picture = Arc::new(Image::from_bytes(ImageFormat::Png, bytes.clone()));
        img(picture).max_w(px(240.)).rounded_md()
    });
    let name = attachment.name.clone();
    let download_view = view.clone();
    let asked = requested.contains(&(message, index));
    let action = if line.own {
        // A sender already has the file.
        div().text_xs().text_color(palette.muted).child("Sent")
    } else {
        div().child(
            Button::new(
                format!("attachment-{:x}-{index}", u128::from_le_bytes(message)),
                if asked { "Requested" } else { "Download" },
            )
            .variant(Variant::Secondary)
            .on_click(move |_, cx| {
                let name = name.clone();
                download_view.update(cx, |view, cx| {
                    if !view.requested.contains(&(message, index)) {
                        view.download(message, index, name, cx);
                    }
                })
            }),
        )
    };
    div()
        .flex()
        .flex_col()
        .gap_1()
        .mt_1()
        .children(preview)
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .gap_2()
                .child(div().text_xs().min_w_0().child(format!(
                    "{} · {}",
                    attachment.name,
                    format_bytes(attachment.bytes)
                )))
                .child(action),
        )
}
