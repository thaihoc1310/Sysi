//! TASKS: a to-do list card. Typed into from a quick-add field that reads
//! dates, lists and priority out of the text (see `crate::tasks`), grouped
//! by when things are due, and rung through desktop notifications.

use super::*;
use crate::state::{Task, TaskStatus};
use crate::tasks::{self, Group, Notice};

pub(super) const TASKS_WIDTH: i32 = 300;
pub(super) const TASKS_HEIGHT: i32 = 340;
/// How long a deleted task can be brought back.
const UNDO_MS: i64 = 6_000;
const SNOOZE_MS: i64 = 10 * 60_000;

#[derive(Clone)]
pub(super) struct TasksCard {
    pub card: gtk::EventBox,
    pub chrome: gtk::EventBox,
    header: gtk::EventBox,
    hide: gtk::Button,
    color_mode: Rc<Cell<Foreground>>,
    resize: ResizeHandle,
    scroller: gtk::ScrolledWindow,
    rows: gtk::Box,
    tab_bar: gtk::Box,
    entry: gtk::Entry,
    preview: gtk::Label,
    status: gtk::Label,
    undo: gtk::Button,
    view: Rc<TasksView>,
}

/// What the card shows beyond the tasks themselves.
#[derive(Default)]
struct TasksView {
    /// The row the keyboard acts on, by task id.
    selected: Cell<Option<u64>>,
    /// The row under the pointer, for its right-click menu.
    hovered: Rc<Cell<Option<u64>>>,
    /// The task being renamed in place; nothing is rebuilt meanwhile.
    editing: Cell<Option<u64>>,
    show_done: Cell<bool>,
    /// An input method is composing in the quick-add field.
    composing: Cell<bool>,
    visible_ids: RefCell<Vec<u64>>,
    last_view: RefCell<String>,
    /// The last task deleted, where it was, and when.
    deleted: RefCell<Option<(usize, Task, i64)>>,
    /// The daily round-up's notification, which opens the card.
    digest_id: Cell<Option<u32>>,
}

/// The card and what it acts on.
#[derive(Clone)]
pub(super) struct Tasks {
    pub card: TasksCard,
    state: Rc<RefCell<AppState>>,
    window: gtk::ApplicationWindow,
    interactive: Rc<Cell<bool>>,
}

pub(super) struct TasksHandle {
    pub tasks: Tasks,
    pub toggle: Rc<dyn Fn()>,
    /// Keys for the card, asked before the window's other handlers.
    pub keys: Rc<dyn Fn(&gdk::EventKey) -> glib::Propagation>,
}

fn build_tasks_window(initial_color_mode: Foreground) -> TasksCard {
    let shell = utility_shell(initial_color_mode, "TASKS", "Hide Tasks", "");
    // Nothing to fetch: the list is all here.
    shell.refresh.set_no_show_all(true);
    shell.refresh.hide();

    let tab_bar = gtk::Box::new(gtk::Orientation::Horizontal, 2);
    tab_bar.style_context().add_class("usage-tabs");

    let entry = gtk::Entry::new();
    entry.set_placeholder_text(Some("+ việc mới: gọi khách mai 3h chiều #work !1"));
    entry.style_context().add_class("history-search");
    entry.style_context().add_class("task-entry");
    entry.set_hexpand(true);
    let preview = gtk::Label::new(None);
    preview.set_xalign(0.0);
    preview.set_ellipsize(gtk::pango::EllipsizeMode::End);
    preview.style_context().add_class("task-preview");
    preview.set_no_show_all(true);
    let quick = gtk::Box::new(gtk::Orientation::Vertical, 1);
    quick.style_context().add_class("task-quick");
    quick.pack_start(&entry, false, false, 0);
    quick.pack_start(&preview, false, false, 0);

    let (scroller, rows) = utility_list(&shell.card);
    let (status, _) = utility_footer("");
    let undo = small_button("Hoàn tác");
    undo.style_context().add_class("task-undo");
    undo.set_no_show_all(true);
    let footer = gtk::Box::new(gtk::Orientation::Horizontal, 3);
    footer.pack_start(&status, true, true, 0);
    footer.pack_end(&undo, false, false, 0);

    let plate = frost_plate();
    let panes = gtk::Box::new(gtk::Orientation::Vertical, 0);
    panes.set_hexpand(true);
    panes.set_vexpand(true);
    panes.pack_start(&tab_bar, false, false, 0);
    panes.pack_start(&quick, false, false, 0);
    panes.pack_start(&scroller, true, true, 0);
    panes.pack_start(&footer, false, false, 0);
    plate.add(&panes);
    shell.body.pack_start(&plate, true, true, 0);
    // The tabs and the field are edit chrome, like the title bar: there is
    // nothing to type into while locked. They follow its visibility.
    shell.chrome.connect_visible_notify({
        let tab_bar = tab_bar.clone();
        let quick = quick.clone();
        move |chrome| {
            tab_bar.set_visible(chrome.is_visible());
            quick.set_visible(chrome.is_visible());
        }
    });
    TasksCard {
        card: shell.card,
        chrome: shell.chrome,
        header: shell.header,
        hide: shell.hide,
        color_mode: shell.color_mode,
        resize: shell.resize,
        scroller,
        rows,
        tab_bar,
        entry,
        preview,
        status,
        undo,
        view: Rc::new(TasksView::default()),
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn install_tasks(
    root: &gtk::Fixed,
    window: &gtk::ApplicationWindow,
    state: &Rc<RefCell<AppState>>,
    registry: &Rc<RefCell<Vec<RegisteredWidget>>>,
    interactive: &Rc<Cell<bool>>,
    picker: &gtk::EventBox,
    default_position: Point,
) -> TasksHandle {
    let color_mode = saved_color_mode(&state.borrow(), "tasks");
    let card = build_tasks_window(foreground_for_mode(color_mode));
    let position = state
        .borrow()
        .positions
        .get("tasks")
        .copied()
        .unwrap_or(default_position);
    apply_widget_size(
        &card.card,
        "tasks",
        state,
        Size {
            width: TASKS_WIDTH,
            height: TASKS_HEIGHT,
        },
    );
    place_card(root, &card.card, position);
    register(
        registry,
        "tasks",
        &card.card,
        card.color_mode.clone(),
        color_mode,
    );
    if let Some(item) = registry
        .borrow_mut()
        .iter_mut()
        .find(|item| item.key == "tasks")
    {
        item.edit_only = Some(card.chrome.clone());
    }
    let tasks = Tasks {
        card: card.clone(),
        state: state.clone(),
        window: window.clone(),
        interactive: interactive.clone(),
    };
    let menu = attach_color_mode_menu(
        &card.card,
        "tasks".into(),
        state.clone(),
        registry.clone(),
        interactive.clone(),
        None,
        Some(build_row_menu(&tasks)),
        None,
        None,
    );
    let clear_done = gtk::MenuItem::with_label("XOÁ VIỆC ĐÃ XONG");
    clear_done.style_context().add_class("menu-destructive");
    clear_done.connect_activate({
        let tasks = tasks.clone();
        move |_| {
            tasks
                .state
                .borrow_mut()
                .tasks
                .retain(|task| task.status != TaskStatus::Done);
            changed(&tasks);
        }
    });
    menu.append(&clear_done);
    clear_done.show();
    attach_drag(
        &card.header,
        &card.card,
        root,
        "tasks".into(),
        state.clone(),
        registry.clone(),
        interactive.clone(),
        window.clone(),
    );
    attach_resize(
        &card.resize,
        &card.card,
        root,
        "tasks".into(),
        state.clone(),
        registry.clone(),
        interactive.clone(),
        window.clone(),
        ResizeBounds {
            min_width: 220,
            min_height: 150,
            max_width: Some(720),
            max_height: Some(1000),
            aspect_ratio: None,
            preserve_current_aspect: false,
            height_for_width: None,
        },
    );
    wire_entry(&tasks);
    card.undo.connect_clicked({
        let tasks = tasks.clone();
        move |_| undo_delete(&tasks)
    });

    let toggle: Rc<dyn Fn()> = {
        let toggle = CardToggle {
            card: card.card.clone(),
            chrome: card.chrome.clone(),
            key: "tasks",
            size: Size {
                width: TASKS_WIDTH,
                height: TASKS_HEIGHT,
            },
            state: state.clone(),
            window: window.clone(),
            registry: registry.clone(),
            interactive: interactive.clone(),
            root: root.clone(),
            picker: picker.clone(),
        };
        let tasks = tasks.clone();
        Rc::new(move || {
            toggle.toggle(
                &|| {
                    render(&tasks);
                    focus_entry(&tasks);
                },
                &|data, open| data.settings.tasks_open = open,
            );
        })
    };
    card.hide.connect_clicked({
        let toggle = toggle.clone();
        move |_| toggle()
    });

    tasks::listen({
        let tasks = tasks.clone();
        let toggle = toggle.clone();
        move |id, action| answer_notification(&tasks, &toggle, id, action)
    });
    // Rung while Sysi was not running: ring now, once.
    pulse(&tasks);

    let keys: Rc<dyn Fn(&gdk::EventKey) -> glib::Propagation> = {
        let tasks = tasks.clone();
        Rc::new(move |event| card_keys(&tasks, event))
    };
    TasksHandle {
        tasks,
        toggle,
        keys,
    }
}

impl Tasks {
    pub(super) fn restore(&self) {
        if self.state.borrow().settings.tasks_open {
            self.card.chrome.set_visible(self.interactive.get());
            render(self);
        } else {
            self.card.card.hide();
        }
    }
}

/// Put the keyboard in the quick-add field, the way a hotkey-opened card
/// has to: the shell activates the overlay, then GTK moves its focus.
fn focus_entry(tasks: &Tasks) {
    if !tasks.interactive.get() {
        return;
    }
    let window = &tasks.window;
    window.set_accept_focus(true);
    request_compositor_focus();
    focus_overlay_for_typing(window);
    window.set_focus_on_map(false);
    window.set_focus(Some(&tasks.card.entry));
    tasks.card.entry.grab_focus();
}

// ---------------------------------------------------------------------------
// Rendering

fn current_tab(state: &AppState) -> Option<String> {
    let tab = &state.settings.tasks_tab;
    (!tab.is_empty() && tasks::lists(&state.tasks).contains(tab)).then(|| tab.clone())
}

fn render(tasks: &Tasks) {
    let view = &tasks.card.view;
    if view.editing.get().is_some() {
        return;
    }
    let now = tasks::now_local();
    let state = tasks.state.borrow();
    let tab = current_tab(&state);
    let lists = tasks::lists(&state.tasks);
    let groups = tasks::group(&state.tasks, tab.as_deref(), now);
    let deleted = view
        .deleted
        .borrow()
        .as_ref()
        .filter(|(_, _, at)| now_ms() - at < UNDO_MS)
        .map(|(_, task, _)| task.title.clone());

    // Everything the rows show, so an unchanged list is not rebuilt.
    let mut key = format!(
        "{:?}|{:?}|{}|{}|{}|{}",
        tab,
        lists,
        view.selected.get().unwrap_or(0),
        view.show_done.get(),
        now.day,
        now.minute
    );
    for (_, members) in &groups {
        for task in members {
            key.push_str(&format!(
                "|{}:{}:{:?}:{}:{}:{:?}:{:?}",
                task.id,
                task.title,
                task.status,
                task.priority,
                task.list,
                task.due_day,
                task.due_minute
            ));
        }
    }
    let open: usize = groups
        .iter()
        .filter(|(group, _)| *group != Group::Done)
        .map(|(_, members)| members.len())
        .sum();
    let overdue = groups
        .iter()
        .find(|(group, _)| *group == Group::Overdue)
        .map_or(0, |(_, members)| members.len());
    tasks.card.status.set_label(&match &deleted {
        Some(title) => format!("Đã xoá \u{201c}{}\u{201d}", ellipsize(title, 24)),
        None if overdue > 0 => format!("{open} việc \u{b7} {overdue} quá hạn"),
        None => format!("{open} việc"),
    });
    tasks.card.undo.set_visible(deleted.is_some());
    if *view.last_view.borrow() == key {
        return;
    }
    *view.last_view.borrow_mut() = key;

    render_tabs(tasks, &lists, tab.as_deref());
    clear_usage_rows(&tasks.card.rows);
    view.hovered.set(None);
    let mut ids = Vec::new();
    let mut selected_row = None;
    if groups.is_empty() {
        let empty = gtk::Label::new(Some("Chưa có việc nào"));
        empty.set_xalign(0.0);
        empty.style_context().add_class("session-detail");
        tasks.card.rows.pack_start(&empty, false, false, 0);
    }
    for (index, (group, members)) in groups.iter().enumerate() {
        if *group == Group::Done {
            done_heading(tasks, members.len(), index > 0);
            if !view.show_done.get() {
                continue;
            }
        } else {
            sessions_heading(&tasks.card.rows, group.label(), index > 0);
        }
        for task in members {
            let row = task_row(tasks, task, *group, tab.is_none(), now);
            if view.selected.get() == Some(task.id) {
                selected_row = Some(row.clone());
            }
            ids.push(task.id);
            tasks.card.rows.pack_start(&row, false, false, 0);
        }
    }
    drop(state);
    if view.selected.get().is_some_and(|id| !ids.contains(&id)) {
        view.selected.set(None);
    }
    *view.visible_ids.borrow_mut() = ids;
    tasks.card.rows.show_all();
    if let Some(row) = selected_row {
        // Laid out on the next pass; bring it into view then.
        let scroller = tasks.card.scroller.clone();
        glib::idle_add_local_once(move || {
            let area = row.allocation();
            scroller
                .vadjustment()
                .clamp_page(f64::from(area.y()), f64::from(area.y() + area.height()));
        });
    }
}

fn render_tabs(tasks: &Tasks, lists: &[String], tab: Option<&str>) {
    let bar = &tasks.card.tab_bar;
    for child in bar.children() {
        bar.remove(&child);
    }
    let mut choices: Vec<(Option<String>, gtk::Button)> = Vec::new();
    for list in std::iter::once(None).chain(lists.iter().cloned().map(Some)) {
        let button = picker_button(list.as_deref().unwrap_or("TẤT CẢ"));
        button.style_context().add_class("usage-tab");
        button.connect_clicked({
            let tasks = tasks.clone();
            let list = list.clone();
            move |_| {
                tasks.state.borrow_mut().settings.tasks_tab = list.clone().unwrap_or_default();
                let _ = tasks.state.borrow().save();
                tasks.card.view.selected.set(None);
                render(&tasks);
            }
        });
        bar.pack_start(&button, false, false, 0);
        // Shown even while locked: the bar itself is what hides with the
        // edit chrome, and it would come back empty.
        button.show();
        choices.push((list, button));
    }
    let active = tab.map(str::to_owned);
    for (list, button) in &choices {
        set_style_class(&button.style_context(), "usage-tab-active", *list == active);
    }
}

fn done_heading(tasks: &Tasks, count: usize, next: bool) {
    let open = tasks.card.view.show_done.get();
    let label = gtk::Label::new(Some(&format!(
        "{} {} ({count})",
        if open { "\u{25be}" } else { "\u{25b8}" },
        Group::Done.label()
    )));
    label.set_xalign(0.0);
    label.style_context().add_class("usage-group-provider");
    if next {
        label.style_context().add_class("usage-group-next");
    }
    let button = gtk::EventBox::new();
    button.set_visible_window(false);
    button.add(&label);
    button.connect_button_release_event({
        let tasks = tasks.clone();
        move |_, event| {
            if event.button() != 1 {
                return glib::Propagation::Proceed;
            }
            let view = &tasks.card.view;
            view.show_done.set(!view.show_done.get());
            let tasks = tasks.clone();
            glib::idle_add_local_once(move || render(&tasks));
            glib::Propagation::Stop
        }
    });
    tasks.card.rows.pack_start(&button, false, false, 0);
}

fn status_mark(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Todo => "\u{2610}",
        TaskStatus::Doing => "\u{25d0}",
        TaskStatus::Done => "\u{2611}",
    }
}

fn task_row(
    tasks: &Tasks,
    task: &Task,
    group: Group,
    show_list: bool,
    now: tasks::Now,
) -> gtk::EventBox {
    let id = task.id;
    let row = gtk::EventBox::new();
    row.set_visible_window(true);
    row.style_context().add_class("notes-row");
    row.style_context().add_class("task-row");
    if tasks.card.view.selected.get() == Some(id) {
        row.style_context().add_class("notes-row-selected");
    }
    row.add_events(
        gdk::EventMask::BUTTON_PRESS_MASK
            | gdk::EventMask::ENTER_NOTIFY_MASK
            | gdk::EventMask::LEAVE_NOTIFY_MASK,
    );
    let line = gtk::Box::new(gtk::Orientation::Horizontal, 5);
    line.style_context().add_class("task-line");

    let mark = gtk::Label::new(Some(status_mark(task.status)));
    mark.style_context().add_class("task-mark");
    let mark_box = gtk::EventBox::new();
    mark_box.set_visible_window(false);
    mark_box.add(&mark);
    mark_box.set_tooltip_text(Some(if task.status == TaskStatus::Done {
        "Chưa xong"
    } else {
        "Xong"
    }));
    mark_box.add_events(gdk::EventMask::BUTTON_PRESS_MASK);
    mark_box.connect_button_press_event({
        let tasks = tasks.clone();
        move |_, event| {
            if event.button() != 1 {
                return glib::Propagation::Proceed;
            }
            let tasks = tasks.clone();
            glib::idle_add_local_once(move || toggle_done(&tasks, id));
            glib::Propagation::Stop
        }
    });
    line.pack_start(&mark_box, false, false, 0);

    if task.priority > 0 {
        let priority = gtk::Label::new(Some(&format!("!{}", task.priority)));
        priority
            .style_context()
            .add_class(&format!("task-priority-{}", task.priority.min(3)));
        line.pack_start(&priority, false, false, 0);
    }
    let title = gtk::Label::new(None);
    let escaped = glib::markup_escape_text(&task.title);
    title.set_markup(&if task.status == TaskStatus::Done {
        format!("<s>{escaped}</s>")
    } else {
        escaped.to_string()
    });
    title.set_xalign(0.0);
    title.set_hexpand(true);
    title.set_ellipsize(gtk::pango::EllipsizeMode::End);
    title.set_tooltip_text(Some(&task.title));
    title.style_context().add_class("task-title");
    if task.status == TaskStatus::Done {
        title.style_context().add_class("task-done");
    }
    line.pack_start(&title, true, true, 0);
    if show_list && !task.list.is_empty() {
        let list = gtk::Label::new(Some(&format!("#{}", task.list)));
        list.style_context().add_class("task-list");
        line.pack_start(&list, false, false, 0);
    }
    if let Some(due) = tasks::due_of(task) {
        let label = gtk::Label::new(Some(&tasks::due_label(due, now)));
        label.style_context().add_class("task-due");
        match group {
            Group::Overdue => label.style_context().add_class("task-overdue"),
            Group::Today => label.style_context().add_class("task-today"),
            _ => {}
        }
        line.pack_end(&label, false, false, 0);
    }
    row.add(&line);

    row.connect_enter_notify_event({
        let hovered = tasks.card.view.hovered.clone();
        move |row, _| {
            hovered.set(Some(id));
            row.style_context().add_class("notes-row-hover");
            glib::Propagation::Proceed
        }
    });
    row.connect_leave_notify_event({
        let hovered = tasks.card.view.hovered.clone();
        move |row, _| {
            if hovered.get() == Some(id) {
                hovered.set(None);
            }
            row.style_context().remove_class("notes-row-hover");
            glib::Propagation::Proceed
        }
    });
    row.connect_button_press_event({
        let tasks = tasks.clone();
        let line = line.clone();
        move |row, event| {
            if event.button() != 1 {
                return glib::Propagation::Proceed;
            }
            if event.event_type() == gdk::EventType::DoubleButtonPress {
                if tasks.interactive.get() {
                    start_edit(&tasks, id, row, &line);
                }
                return glib::Propagation::Stop;
            }
            tasks.card.view.selected.set(Some(id));
            for child in tasks.card.rows.children() {
                set_style_class(
                    &child.style_context(),
                    "notes-row-selected",
                    child == *row.upcast_ref::<gtk::Widget>(),
                );
            }
            // Clicked, the keyboard follows: Space and Delete act on it.
            if tasks.interactive.get() && !tasks.card.entry.has_focus() {
                focus_entry(&tasks);
            }
            glib::Propagation::Stop
        }
    });
    row
}

// ---------------------------------------------------------------------------
// Changes

/// After any change to the tasks: keep it, redraw, and set the next alarm.
fn changed(tasks: &Tasks) {
    let _ = tasks.state.borrow().save();
    render(tasks);
    arm(tasks);
}

fn with_task(tasks: &Tasks, id: u64, change: impl FnOnce(&mut Task)) -> bool {
    let mut state = tasks.state.borrow_mut();
    let Some(task) = state.tasks.iter_mut().find(|task| task.id == id) else {
        return false;
    };
    change(task);
    true
}

/// Take a task's notification down, if one is up.
fn withdraw(task: &mut Task) {
    if let Some(id) = task.notify_id.take() {
        tasks::close_notification(id);
    }
}

fn set_status(tasks: &Tasks, id: u64, status: TaskStatus) {
    if with_task(tasks, id, |task| {
        task.status = status;
        if status == TaskStatus::Done {
            task.completed_at = Some(now_ms());
            withdraw(task);
        } else {
            task.completed_at = None;
        }
    }) {
        changed(tasks);
    }
}

fn toggle_done(tasks: &Tasks, id: u64) {
    let done = tasks
        .state
        .borrow()
        .tasks
        .iter()
        .find(|task| task.id == id)
        .map(|task| task.status == TaskStatus::Done);
    if let Some(done) = done {
        set_status(
            tasks,
            id,
            if done {
                TaskStatus::Todo
            } else {
                TaskStatus::Done
            },
        );
    }
}

fn delete(tasks: &Tasks, id: u64) {
    let removed = {
        let mut state = tasks.state.borrow_mut();
        let index = state.tasks.iter().position(|task| task.id == id);
        index.map(|index| (index, state.tasks.remove(index)))
    };
    let Some((index, mut task)) = removed else {
        return;
    };
    withdraw(&mut task);
    *tasks.card.view.deleted.borrow_mut() = Some((index, task, now_ms()));
    // The footer's undo goes away on its own.
    glib::timeout_add_local_once(Duration::from_millis(UNDO_MS as u64 + 50), {
        let tasks = tasks.clone();
        move || render(&tasks)
    });
    changed(tasks);
}

fn undo_delete(tasks: &Tasks) {
    let Some((index, task, at)) = tasks.card.view.deleted.borrow_mut().take() else {
        return;
    };
    if now_ms() - at >= UNDO_MS {
        return;
    }
    let mut state = tasks.state.borrow_mut();
    let index = index.min(state.tasks.len());
    state.tasks.insert(index, task);
    drop(state);
    changed(tasks);
}

/// Give a task the due date of `due`, with the reminder that goes with it.
fn set_due(task: &mut Task, due: Option<tasks::Due>) {
    let before = (task.due_day, task.due_minute);
    task.due_day = due.map(|due| due.day);
    task.due_minute = due.and_then(|due| due.minute);
    if before != (task.due_day, task.due_minute) {
        withdraw(task);
        // A time already gone has nothing left to ring for.
        task.remind_at = tasks::remind_at(due).filter(|at| *at > now_ms());
        task.reminded = false;
    }
}

fn add_from_entry(tasks: &Tasks) {
    let text = tasks.card.entry.text().to_string();
    let now = tasks::now_local();
    let draft = tasks::parse(&text, now);
    if draft.title.trim().is_empty() {
        return;
    }
    {
        let mut state = tasks.state.borrow_mut();
        let existing = tasks::lists(&state.tasks);
        let list = match &draft.list {
            Some(name) => tasks::resolve_list(name, &existing),
            // Typed under a list's tab: it goes in that list.
            None => current_tab(&state).unwrap_or_default(),
        };
        let id = state.next_task_id.max(1);
        state.next_task_id = id + 1;
        let mut task = Task {
            id,
            title: draft.title.trim().to_owned(),
            list,
            priority: draft.priority,
            created_at: now_ms(),
            ..Task::default()
        };
        set_due(&mut task, draft.due);
        state.tasks.push(task);
    }
    tasks.card.entry.set_text("");
    changed(tasks);
}

// ---------------------------------------------------------------------------
// Editing in place

/// The line an edit starts from: the title, then its date, list and
/// priority written the way the quick-add field reads them back.
fn edit_text(task: &Task, now: tasks::Now) -> String {
    let title = if tasks::parse(&task.title, now).title == task.title {
        task.title.clone()
    } else {
        // Words in it that would read as a date stay words.
        format!("\"{}\"", task.title)
    };
    let mut parts = vec![title];
    if let Some(day) = task.due_day {
        let (year, month, date) = crate::usage::civil_from_days(day);
        parts.push(format!("{date}/{month}/{year}"));
    }
    if let Some(minute) = task.due_minute {
        parts.push(format!("{:02}:{:02}", minute / 60, minute % 60));
    }
    if !task.list.is_empty() {
        parts.push(format!("#{}", task.list));
    }
    if task.priority > 0 {
        parts.push(format!("!{}", task.priority));
    }
    parts.join(" ")
}

fn start_edit(tasks: &Tasks, id: u64, row: &gtk::EventBox, line: &gtk::Box) {
    let Some(text) = tasks
        .state
        .borrow()
        .tasks
        .iter()
        .find(|task| task.id == id)
        .map(|task| edit_text(task, tasks::now_local()))
    else {
        return;
    };
    tasks.card.view.editing.set(Some(id));
    let field = gtk::Entry::new();
    field.set_text(&text);
    field.style_context().add_class("history-search");
    field.style_context().add_class("task-entry");
    field.set_hexpand(true);
    row.remove(line);
    row.add(&field);
    field.show();
    focus_overlay_for_typing(&tasks.window);
    field.grab_focus();
    let finish = {
        let tasks = tasks.clone();
        let field = field.clone();
        Rc::new(move |save: bool| {
            if tasks.card.view.editing.replace(None) != Some(id) {
                return;
            }
            if save {
                let draft = tasks::parse(&field.text(), tasks::now_local());
                if !draft.title.trim().is_empty() {
                    let existing = tasks::lists(&tasks.state.borrow().tasks);
                    with_task(&tasks, id, |task| {
                        task.title = draft.title.trim().to_owned();
                        task.priority = draft.priority;
                        task.list = draft
                            .list
                            .as_deref()
                            .map(|name| tasks::resolve_list(name, &existing))
                            .unwrap_or_default();
                        set_due(task, draft.due);
                    });
                }
            }
            tasks.card.view.last_view.borrow_mut().clear();
            changed(&tasks);
            focus_entry(&tasks);
        })
    };
    field.connect_activate({
        let finish = finish.clone();
        move |_| finish(true)
    });
    field.connect_key_press_event({
        let finish = finish.clone();
        move |_, event| {
            if event.keyval() == gdk::keys::constants::Escape {
                finish(false);
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        }
    });
    field.connect_focus_out_event(move |_, _| {
        // Clicked away: keep what was typed. After the click is done with,
        // since this row is torn down by the redraw.
        let finish = finish.clone();
        glib::idle_add_local_once(move || finish(true));
        glib::Propagation::Proceed
    });
}

// ---------------------------------------------------------------------------
// Keys and the quick-add field

fn wire_entry(tasks: &Tasks) {
    let entry = &tasks.card.entry;
    entry.connect_changed({
        let tasks = tasks.clone();
        move |entry| {
            let text = entry.text();
            let line = if text.trim().is_empty() {
                String::new()
            } else {
                tasks::preview(&tasks::parse(&text, tasks::now_local()), tasks::now_local())
            };
            tasks.card.preview.set_label(&line);
            tasks.card.preview.set_visible(!line.is_empty());
        }
    });
    entry.connect_preedit_changed({
        let view = tasks.card.view.clone();
        move |_, preedit| view.composing.set(!preedit.is_empty())
    });
    entry.connect_activate({
        let tasks = tasks.clone();
        move |entry| {
            if entry.text().is_empty() {
                if let Some(id) = tasks.card.view.selected.get() {
                    edit_selected(&tasks, id);
                }
                return;
            }
            add_from_entry(&tasks);
        }
    });
}

fn edit_selected(tasks: &Tasks, id: u64) {
    let index = tasks
        .card
        .view
        .visible_ids
        .borrow()
        .iter()
        .position(|candidate| *candidate == id);
    let Some(index) = index else {
        return;
    };
    // The rows box holds headings too: find the row by its place among rows.
    let rows: Vec<gtk::EventBox> = tasks
        .card
        .rows
        .children()
        .into_iter()
        .filter(|child| child.style_context().has_class("task-row"))
        .filter_map(|child| child.downcast::<gtk::EventBox>().ok())
        .collect();
    let Some(row) = rows.get(index) else {
        return;
    };
    let Some(line) = row
        .child()
        .and_then(|line| line.downcast::<gtk::Box>().ok())
    else {
        return;
    };
    start_edit(tasks, id, row, &line);
}

/// Keys while the quick-add field has the keyboard. Moving through the rows
/// and acting on the selected one only happens with the field empty and no
/// word being composed, so typing (and the input method) keep every key.
fn card_keys(tasks: &Tasks, event: &gdk::EventKey) -> glib::Propagation {
    use gdk::keys::constants as key;
    let card = &tasks.card;
    if !card.entry.has_focus() || card.view.editing.get().is_some() {
        return glib::Propagation::Proceed;
    }
    let view = &card.view;
    let keyval = event.keyval();
    let idle = card.entry.text().is_empty() && !view.composing.get();
    let ctrl = event.state().contains(gdk::ModifierType::CONTROL_MASK);
    match keyval {
        key::Up | key::Down if !view.composing.get() => {
            let ids = view.visible_ids.borrow().clone();
            if ids.is_empty() {
                return glib::Propagation::Stop;
            }
            let at = view
                .selected
                .get()
                .and_then(|id| ids.iter().position(|candidate| *candidate == id));
            let next = match (at, keyval == key::Down) {
                (None, true) => 0,
                (None, false) => ids.len() - 1,
                (Some(at), true) => (at + 1).min(ids.len() - 1),
                (Some(at), false) => at.saturating_sub(1),
            };
            view.selected.set(Some(ids[next]));
            render(tasks);
            glib::Propagation::Stop
        }
        key::Escape if view.selected.get().is_some() && idle => {
            view.selected.set(None);
            render(tasks);
            glib::Propagation::Stop
        }
        key::Escape if !card.entry.text().is_empty() && !view.composing.get() => {
            card.entry.set_text("");
            glib::Propagation::Stop
        }
        key::space if idle => {
            let Some(id) = view.selected.get() else {
                return glib::Propagation::Proceed;
            };
            toggle_done(tasks, id);
            glib::Propagation::Stop
        }
        key::Delete | key::KP_Delete if idle => {
            let Some(id) = view.selected.get() else {
                return glib::Propagation::Proceed;
            };
            // The next row takes the selection.
            let ids = view.visible_ids.borrow().clone();
            let at = ids.iter().position(|candidate| *candidate == id);
            view.selected.set(at.and_then(|at| {
                ids.get(at + 1)
                    .or_else(|| at.checked_sub(1).and_then(|at| ids.get(at)))
                    .copied()
            }));
            delete(tasks, id);
            glib::Propagation::Stop
        }
        key::z | key::Z if ctrl && idle && view.deleted.borrow().is_some() => {
            undo_delete(tasks);
            glib::Propagation::Stop
        }
        _ => glib::Propagation::Proceed,
    }
}

// ---------------------------------------------------------------------------
// The right-click menu of a row

fn build_row_menu(tasks: &Tasks) -> Rc<dyn Fn() -> bool> {
    let menu = context_menu();
    let target: Rc<Cell<Option<u64>>> = Rc::new(Cell::new(None));
    let act = |label: &str, action: Rc<dyn Fn(&Tasks, u64)>| {
        let item = gtk::MenuItem::with_label(label);
        item.connect_activate({
            let tasks = tasks.clone();
            let target = target.clone();
            move |_| {
                if let Some(id) = target.get() {
                    action(&tasks, id);
                }
            }
        });
        item
    };

    let doing = act(
        "ĐANG LÀM",
        Rc::new(|tasks, id| {
            let status = tasks
                .state
                .borrow()
                .tasks
                .iter()
                .find(|task| task.id == id)
                .map(|task| task.status);
            set_status(
                tasks,
                id,
                if status == Some(TaskStatus::Doing) {
                    TaskStatus::Todo
                } else {
                    TaskStatus::Doing
                },
            );
        }),
    );

    let priority = gtk::MenuItem::with_label("ƯU TIÊN");
    let priorities = context_menu();
    for (label, level) in [("!1", 1u8), ("!2", 2), ("!3", 3), ("KHÔNG", 0)] {
        priorities.append(&act(
            label,
            Rc::new(move |tasks, id| {
                if with_task(tasks, id, |task| task.priority = level) {
                    changed(tasks);
                }
            }),
        ));
    }
    priorities.show_all();
    priority.set_submenu(Some(&priorities));

    // Refilled each time: the lists come and go with their tasks.
    let list = gtk::MenuItem::with_label("DANH SÁCH");
    let lists_menu = context_menu();
    list.set_submenu(Some(&lists_menu));

    let edit = act(
        "SỬA",
        Rc::new(|tasks, id| {
            let tasks = tasks.clone();
            // After the menu has let go of the pointer.
            glib::idle_add_local_once(move || edit_selected_or(&tasks, id));
        }),
    );
    let remove = act("XOÁ", Rc::new(delete));
    remove.style_context().add_class("menu-destructive");
    for item in [&doing, &priority, &list, &edit, &remove] {
        menu.append(item);
    }
    menu.show_all();

    let tasks = tasks.clone();
    Rc::new(move || {
        let Some(id) = tasks.card.view.hovered.get() else {
            return false;
        };
        let Some(task) = tasks
            .state
            .borrow()
            .tasks
            .iter()
            .find(|task| task.id == id)
            .cloned()
        else {
            return false;
        };
        doing.set_label(if task.status == TaskStatus::Doing {
            "TODO"
        } else {
            "ĐANG LÀM"
        });
        doing.set_visible(task.status != TaskStatus::Done);
        for child in lists_menu.children() {
            lists_menu.remove(&child);
        }
        let lists = tasks::lists(&tasks.state.borrow().tasks);
        for name in std::iter::once(String::new()).chain(lists) {
            let label = if name.is_empty() {
                "KHÔNG CÓ".to_owned()
            } else {
                format!("#{name}")
            };
            let item = gtk::MenuItem::with_label(&label);
            item.set_sensitive(name != task.list);
            item.connect_activate({
                let tasks = tasks.clone();
                move |_| {
                    if with_task(&tasks, id, |task| task.list = name.clone()) {
                        changed(&tasks);
                    }
                }
            });
            lists_menu.append(&item);
        }
        lists_menu.show_all();
        target.set(Some(id));
        menu.popup_easy(3, gtk::current_event_time());
        true
    })
}

fn edit_selected_or(tasks: &Tasks, id: u64) {
    tasks.card.view.selected.set(Some(id));
    tasks.card.view.last_view.borrow_mut().clear();
    render(tasks);
    edit_selected(tasks, id);
}

// ---------------------------------------------------------------------------
// Reminders

thread_local! {
    static PULSE: RefCell<Option<glib::SourceId>> = const { RefCell::new(None) };
}

/// Wake for the next reminder, or at the next minute to move the labels on,
/// whichever is first; never later than a minute, so a suspend that skips
/// past a reminder is caught on waking.
fn arm(tasks: &Tasks) {
    if let Some(source) = PULSE.with(|pulse| pulse.borrow_mut().take()) {
        source.remove();
    }
    let now = now_ms();
    let next_minute = 60_000 - now.rem_euclid(60_000);
    let wait = tasks::next_reminder(&tasks.state.borrow().tasks)
        .map_or(next_minute, |at| (at - now).clamp(0, next_minute))
        .max(200);
    let source = glib::timeout_add_local_once(Duration::from_millis(wait as u64), {
        let tasks = tasks.clone();
        move || {
            // Fired: its id is spent and must not be removed again.
            PULSE.with(|pulse| pulse.borrow_mut().take());
            pulse(&tasks);
        }
    });
    PULSE.with(|pulse| *pulse.borrow_mut() = Some(source));
}

/// Ring what is due, send the day's round-up when its time comes, redraw.
fn pulse(tasks: &Tasks) {
    ring_due(tasks);
    send_digest(tasks);
    if tasks.card.card.is_visible() {
        render(tasks);
    }
    arm(tasks);
}

fn ring_due(tasks: &Tasks) {
    let now = now_ms();
    let today = tasks::now_local();
    let due: Vec<(u64, String, String)> = {
        let mut state = tasks.state.borrow_mut();
        let mut due = Vec::new();
        for task in &mut state.tasks {
            let Some(at) = task.remind_at else {
                continue;
            };
            if task.status == TaskStatus::Done || task.reminded || at > now {
                continue;
            }
            task.reminded = true;
            let mut body = tasks::due_of(task)
                .map(|due| tasks::due_label(due, today))
                .unwrap_or_default();
            if !task.list.is_empty() {
                body.push_str(&format!(" \u{b7} #{}", task.list));
            }
            // Missed while Sysi was not running, or asleep.
            if now - at > 2 * 60_000 {
                body.push_str(" (trễ)");
            }
            due.push((task.id, task.title.clone(), body));
        }
        due
    };
    if due.is_empty() {
        return;
    }
    let _ = tasks.state.borrow().save();
    for (id, title, body) in due {
        tasks::notify(
            Notice {
                summary: &title,
                body: &body,
                actions: &[
                    ("done", "Xong"),
                    ("snooze", "Nhắc lại 10 phút"),
                    ("default", "Mở"),
                ],
                alarm: true,
            },
            {
                let tasks = tasks.clone();
                move |notify_id| {
                    if with_task(&tasks, id, |task| task.notify_id = notify_id) {
                        let _ = tasks.state.borrow().save();
                    }
                }
            },
        );
    }
}

fn send_digest(tasks: &Tasks) {
    let now = tasks::now_local();
    let sent = tasks.state.borrow().settings.tasks_digest_sent;
    let Some(slot) = tasks::digest_due(now, sent) else {
        return;
    };
    tasks.state.borrow_mut().settings.tasks_digest_sent = Some((now.day, slot));
    let _ = tasks.state.borrow().save();
    let (today, overdue, titles) = {
        let state = tasks.state.borrow();
        let groups = tasks::group(&state.tasks, None, now);
        let count = |wanted: Group| {
            groups
                .iter()
                .find(|(group, _)| *group == wanted)
                .map_or(0, |(_, members)| members.len())
        };
        let titles: Vec<String> = groups
            .iter()
            .filter(|(group, _)| matches!(group, Group::Overdue | Group::Today))
            .flat_map(|(_, members)| members.iter())
            .take(5)
            .map(|task| format!("\u{2022} {}", task.title))
            .collect();
        (count(Group::Today), count(Group::Overdue), titles)
    };
    if today + overdue == 0 {
        return;
    }
    let summary = if overdue > 0 {
        format!("Hôm nay {today} việc \u{b7} {overdue} quá hạn")
    } else {
        format!("Hôm nay {today} việc")
    };
    let body = titles.join("\n");
    let view = tasks.card.view.clone();
    tasks::notify(
        Notice {
            summary: &summary,
            body: &body,
            actions: &[("default", "Mở TASKS")],
            alarm: false,
        },
        move |id| view.digest_id.set(id),
    );
}

/// A button pressed on one of Sysi's notifications.
fn answer_notification(tasks: &Tasks, toggle: &Rc<dyn Fn()>, notify_id: u32, action: &str) {
    let open = |tasks: &Tasks| {
        if !tasks.window.is_visible() {
            tasks.window.show();
        }
        if !tasks.card.card.is_visible() {
            toggle();
        }
    };
    if tasks.card.view.digest_id.get() == Some(notify_id) {
        tasks.card.view.digest_id.set(None);
        if action == "default" {
            open(tasks);
        }
        return;
    }
    let id = tasks
        .state
        .borrow()
        .tasks
        .iter()
        .find(|task| task.notify_id == Some(notify_id))
        .map(|task| task.id);
    let Some(id) = id else {
        return;
    };
    match action {
        "done" => set_status(tasks, id, TaskStatus::Done),
        "snooze" => {
            if with_task(tasks, id, |task| {
                task.notify_id = None;
                task.remind_at = Some(now_ms() + SNOOZE_MS);
                task.reminded = false;
            }) {
                changed(tasks);
            }
        }
        "default" => {
            with_task(tasks, id, |task| task.notify_id = None);
            tasks.card.view.selected.set(Some(id));
            open(tasks);
            render(tasks);
        }
        _ => {}
    }
}
