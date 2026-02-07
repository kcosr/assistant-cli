//! Assistant CLI - Lists Browser
//!
//! A TUI for browsing and managing Assistant lists.
//!
//! Run with:
//! ```bash
//! ASSISTANT_URL=http://localhost:3000 lists
//! ```

#![allow(dead_code)]

use std::cell::Cell;
use std::env;
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread;
use std::time::Duration;

use ftui_core::event::{Event, KeyCode, Modifiers, MouseButton, MouseEventKind};
use ftui_core::geometry::Rect;
use ftui_layout::{Constraint, Flex};
use ftui_render::cell::Cell as RenderCell;
use ftui_render::frame::Frame;
use ftui_runtime::{Cmd, Model, Program, ProgramConfig, ScreenMode};
use ftui_render::cell::PackedRgba;
use ftui_style::Style;
use ftui_widgets::block::Block;
use ftui_widgets::borders::{BorderType, Borders};
use ftui_widgets::paragraph::Paragraph;
use ftui_widgets::table::{Table, Row, TableState};
use ftui_widgets::modal::{Dialog, DialogState, DialogResult};
use ftui_widgets::{StatefulWidget, Widget};
use serde::Deserialize;
use tungstenite::{connect, Message as WsMessage};
use url::Url;

// ============================================================================
// API Types
// ============================================================================

#[derive(Debug, Deserialize)]
struct ApiResponse<T> {
    ok: bool,
    result: Option<T>,
    error: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListInfo {
    id: String,
    name: String,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    description: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListItem {
    id: String,
    title: String,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    notes: Option<String>,
    #[serde(default)]
    completed: Option<bool>,
}

// ============================================================================
// WebSocket Types
// ============================================================================

/// Events received from the WebSocket connection
#[derive(Debug, Clone)]
enum WsEvent {
    Connected,
    Disconnected(String),
    ListUpdate { list_id: String, action: String },
}

/// Payload structure for panel events
#[derive(Debug, Deserialize)]
struct PanelEventPayload {
    #[serde(rename = "type")]
    event_type: Option<String>,
    #[serde(rename = "listId")]
    list_id: Option<String>,
    action: Option<String>,
}

/// Server message envelope
#[derive(Debug, Deserialize)]
struct ServerMessage {
    #[serde(rename = "type")]
    msg_type: String,
    #[serde(rename = "panelType")]
    panel_type: Option<String>,
    payload: Option<PanelEventPayload>,
}

// ============================================================================
// API Functions
// ============================================================================

fn api_post<T: for<'de> Deserialize<'de>>(base_url: &str, endpoint: &str, body: &str) -> Result<T, String> {
    let url = format!("{}/api/plugins/lists/operations/{}", base_url, endpoint);
    
    let output = Command::new("curl")
        .args(["-s", "-X", "POST", "-H", "Content-Type: application/json", "-d", body, &url])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .map_err(|e| format!("curl failed: {}", e))?;
    
    let response_body = String::from_utf8_lossy(&output.stdout);
    let response: ApiResponse<T> = serde_json::from_str(&response_body)
        .map_err(|e| format!("JSON parse error: {} (body: {})", e, &response_body[..response_body.len().min(200)]))?;
    
    if response.ok {
        response.result.ok_or_else(|| "Empty result".to_string())
    } else {
        Err(response.error.unwrap_or_else(|| "Unknown error".to_string()))
    }
}

fn fetch_lists(base_url: &str) -> Result<Vec<ListInfo>, String> {
    api_post(base_url, "list", "{}")
}

fn fetch_items(base_url: &str, list_id: &str, limit: usize) -> Result<Vec<ListItem>, String> {
    let body = serde_json::json!({
        "listId": list_id,
        "limit": limit
    }).to_string();
    api_post(base_url, "items-list", &body)
}

fn search_items(base_url: &str, query: &str, list_id: Option<&str>, limit: usize) -> Result<Vec<ListItem>, String> {
    let body = if let Some(id) = list_id {
        serde_json::json!({
            "query": query,
            "listId": id,
            "limit": limit
        })
    } else {
        serde_json::json!({
            "query": query,
            "limit": limit
        })
    }.to_string();
    api_post(base_url, "items-search", &body)
}

fn aql_query(base_url: &str, list_id: &str, query: &str, limit: usize) -> Result<Vec<ListItem>, String> {
    let body = serde_json::json!({
        "listId": list_id,
        "query": query,
        "limit": limit
    }).to_string();
    api_post(base_url, "items-aql", &body)
}

fn delete_item(base_url: &str, list_id: &str, item_id: &str) -> Result<(), String> {
    let body = serde_json::json!({
        "listId": list_id,
        "id": item_id
    }).to_string();
    let _: serde_json::Value = api_post(base_url, "item-remove", &body)?;
    Ok(())
}

fn add_item(base_url: &str, list_id: &str, title: &str) -> Result<ListItem, String> {
    let body = serde_json::json!({
        "listId": list_id,
        "title": title
    }).to_string();
    api_post(base_url, "item-add", &body)
}

fn update_item_completed(base_url: &str, list_id: &str, item_id: &str, completed: bool) -> Result<ListItem, String> {
    let body = serde_json::json!({
        "listId": list_id,
        "id": item_id,
        "completed": completed
    }).to_string();
    api_post(base_url, "item-update", &body)
}

fn update_item_position(base_url: &str, list_id: &str, item_id: &str, position: usize) -> Result<ListItem, String> {
    let body = serde_json::json!({
        "listId": list_id,
        "id": item_id,
        "position": position
    }).to_string();
    api_post(base_url, "item-update", &body)
}

// ============================================================================
// WebSocket Connection
// ============================================================================

fn spawn_websocket_thread(base_url: String, tx: Sender<WsEvent>) {
    thread::spawn(move || {
        loop {
            // Convert HTTP URL to WebSocket URL
            let ws_url = base_url
                .replace("http://", "ws://")
                .replace("https://", "wss://")
                + "/ws";
            
            let url = match Url::parse(&ws_url) {
                Ok(u) => u,
                Err(e) => {
                    let _ = tx.send(WsEvent::Disconnected(format!("Invalid URL: {}", e)));
                    thread::sleep(Duration::from_secs(5));
                    continue;
                }
            };
            
            // Try to connect
            let socket = match connect(url) {
                Ok((socket, _response)) => {
                    let _ = tx.send(WsEvent::Connected);
                    socket
                }
                Err(e) => {
                    let _ = tx.send(WsEvent::Disconnected(format!("Connect failed: {}", e)));
                    thread::sleep(Duration::from_secs(5));
                    continue;
                }
            };
            
            // Send hello message
            let hello = serde_json::json!({
                "type": "hello",
                "protocolVersion": 2,
                "userAgent": "lists-tui/1.0"
            });
            
            let mut socket = socket;
            if let Err(e) = socket.send(WsMessage::Text(hello.to_string())) {
                let _ = tx.send(WsEvent::Disconnected(format!("Send hello failed: {}", e)));
                thread::sleep(Duration::from_secs(5));
                continue;
            }
            
            // Read messages
            loop {
                match socket.read() {
                    Ok(WsMessage::Text(text)) => {
                        // Parse the message
                        if let Ok(msg) = serde_json::from_str::<ServerMessage>(&text) {
                            if msg.msg_type == "panel_event" {
                                if let Some(ref panel_type) = msg.panel_type {
                                    if panel_type == "lists" {
                                        if let Some(ref payload) = msg.payload {
                                            if let (Some(list_id), Some(action)) = 
                                                (&payload.list_id, &payload.action) 
                                            {
                                                let _ = tx.send(WsEvent::ListUpdate {
                                                    list_id: list_id.clone(),
                                                    action: action.clone(),
                                                });
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Ok(WsMessage::Ping(data)) => {
                        let _ = socket.send(WsMessage::Pong(data));
                    }
                    Ok(WsMessage::Close(_)) => {
                        let _ = tx.send(WsEvent::Disconnected("Server closed connection".to_string()));
                        break;
                    }
                    Err(e) => {
                        let _ = tx.send(WsEvent::Disconnected(format!("Read error: {}", e)));
                        break;
                    }
                    _ => {}
                }
            }
            
            // Reconnect after a delay
            thread::sleep(Duration::from_secs(3));
        }
    });
}

// ============================================================================
// Colors
// ============================================================================

mod colors {
    use super::*;
    
    pub const BG_PRIMARY: PackedRgba = PackedRgba::rgb(20, 20, 30);
    pub const BG_SURFACE: PackedRgba = PackedRgba::rgb(30, 30, 45);
    pub const BG_SELECTED: PackedRgba = PackedRgba::rgb(60, 60, 90);
    pub const BG_HEADER: PackedRgba = PackedRgba::rgb(40, 40, 60);
    
    pub const FG_PRIMARY: PackedRgba = PackedRgba::rgb(220, 220, 240);
    pub const FG_SECONDARY: PackedRgba = PackedRgba::rgb(160, 160, 180);
    pub const FG_MUTED: PackedRgba = PackedRgba::rgb(100, 100, 120);
    pub const FG_SUCCESS: PackedRgba = PackedRgba::rgb(100, 220, 100);
    pub const FG_ERROR: PackedRgba = PackedRgba::rgb(255, 100, 100);
    
    pub const ACCENT_PRIMARY: PackedRgba = PackedRgba::rgb(100, 180, 255);
    pub const ACCENT_TAG: PackedRgba = PackedRgba::rgb(255, 180, 100);
    pub const ACCENT_URL: PackedRgba = PackedRgba::rgb(150, 200, 255);
    pub const ACCENT_COMPLETED: PackedRgba = PackedRgba::rgb(100, 200, 150);
    
    pub const BORDER_FOCUSED: PackedRgba = PackedRgba::rgb(100, 180, 255);
    pub const BORDER_UNFOCUSED: PackedRgba = PackedRgba::rgb(80, 80, 100);
}

// ============================================================================
// App State
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum View {
    Lists,
    Items,
    Search,
    AqlQuery,
    Detail,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Sidebar,
    Table,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputMode {
    Normal,
    Search,
    AddItem,
    AqlQuery,
    ListFilter,
}

/// Columns that can be shown in the table
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Column {
    Status,
    Title,
    Tags,
    Url,
    Notes,
}

impl Column {
    fn from_str(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "status" | "completed" => Some(Column::Status),
            "title" | "name" => Some(Column::Title),
            "tags" | "tag" => Some(Column::Tags),
            "url" | "link" => Some(Column::Url),
            "notes" | "note" | "description" => Some(Column::Notes),
            _ => None,
        }
    }
    
    fn header(&self) -> &'static str {
        match self {
            Column::Status => "",
            Column::Title => "Title",
            Column::Tags => "Tags",
            Column::Url => "URL",
            Column::Notes => "📝",
        }
    }
    
    fn aql_field(&self) -> &'static str {
        match self {
            Column::Status => "completed",
            Column::Title => "title",
            Column::Tags => "tags",
            Column::Url => "url",
            Column::Notes => "notes",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SortDirection {
    Asc,
    Desc,
}

impl SortDirection {
    fn toggle(self) -> Self {
        match self {
            SortDirection::Asc => SortDirection::Desc,
            SortDirection::Desc => SortDirection::Asc,
        }
    }
    
    fn indicator(self) -> &'static str {
        match self {
            SortDirection::Asc => "▲",
            SortDirection::Desc => "▼",
        }
    }
    
    fn aql_suffix(self) -> &'static str {
        match self {
            SortDirection::Asc => "asc",
            SortDirection::Desc => "desc",
        }
    }
}

struct ListsTableBrowser {
    base_url: String,
    
    // Data
    lists: Vec<ListInfo>,
    items: Vec<ListItem>,
    search_results: Vec<ListItem>,
    aql_results: Vec<ListItem>,
    
    // Selection state
    list_index: usize,
    table_state: TableState,
    search_table_state: TableState,
    aql_table_state: TableState,
    
    // View state
    view: View,
    focus: Focus,
    input_mode: InputMode,
    search_query: String,
    add_item_text: String,
    aql_query: String,
    list_filter: String,
    aql_columns: Option<Vec<Column>>,  // Columns from `show` clause
    sort_column: Option<Column>,
    sort_direction: SortDirection,
    
    // Detail view
    detail_item: Option<ListItem>,
    detail_scroll: usize,
    
    // Layout areas for mouse
    sidebar_area: Cell<Rect>,
    table_area: Cell<Rect>,
    column_positions: Cell<Vec<(Column, u16, u16)>>,  // (column, start_x, end_x)
    
    // WebSocket
    ws_receiver: Option<Receiver<WsEvent>>,
    ws_connected: bool,
    
    // Delete confirmation dialog
    delete_confirm: Option<(String, String, String)>,  // (list_id, item_id, item_title)
    dialog_state: DialogState,
    
    // Messages
    status_message: Option<String>,
    error_message: Option<String>,
}

enum Msg {
    Event(Event),
}

impl From<Event> for Msg {
    fn from(event: Event) -> Self {
        Msg::Event(event)
    }
}

impl ListsTableBrowser {
    fn new() -> Self {
        let base_url = env::var("ASSISTANT_URL")
            .unwrap_or_else(|_| "http://localhost:3000".to_string());
        
        // Create channel for WebSocket events
        let (ws_tx, ws_rx) = mpsc::channel();
        
        // Spawn WebSocket thread
        spawn_websocket_thread(base_url.clone(), ws_tx);
        
        let mut app = Self {
            base_url,
            lists: Vec::new(),
            items: Vec::new(),
            search_results: Vec::new(),
            aql_results: Vec::new(),
            list_index: 0,
            table_state: TableState::default(),
            search_table_state: TableState::default(),
            aql_table_state: TableState::default(),
            view: View::Lists,
            focus: Focus::Sidebar,
            input_mode: InputMode::Normal,
            search_query: String::new(),
            add_item_text: String::new(),
            aql_query: String::new(),
            list_filter: String::new(),
            aql_columns: None,
            sort_column: None,
            sort_direction: SortDirection::Desc,
            detail_item: None,
            detail_scroll: 0,
            sidebar_area: Cell::new(Rect::default()),
            table_area: Cell::new(Rect::default()),
            column_positions: Cell::new(Vec::new()),
            ws_receiver: Some(ws_rx),
            ws_connected: false,
            delete_confirm: None,
            dialog_state: DialogState::default(),
            status_message: None,
            error_message: None,
        };
        
        app.refresh_lists();
        app
    }
    
    /// Poll WebSocket events (non-blocking)
    fn poll_ws_events(&mut self) {
        // Collect events first to avoid borrow issues
        let events: Vec<WsEvent> = {
            let receiver = match self.ws_receiver.as_ref() {
                Some(rx) => rx,
                None => return,
            };
            
            let mut events = Vec::new();
            loop {
                match receiver.try_recv() {
                    Ok(event) => events.push(event),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        // Mark for cleanup after the loop
                        events.push(WsEvent::Disconnected("Channel closed".to_string()));
                        break;
                    }
                }
            }
            events
        };
        
        // Now process events with mutable access
        let mut should_refresh_items = false;
        let mut should_refresh_lists = false;
        let mut last_action = String::new();
        
        for event in events {
            match event {
                WsEvent::Connected => {
                    self.ws_connected = true;
                    self.status_message = Some("🔗 Connected".to_string());
                }
                WsEvent::Disconnected(reason) => {
                    self.ws_connected = false;
                    if reason == "Channel closed" {
                        self.ws_receiver = None;
                    }
                    self.status_message = Some(format!("⚠ {}", reason));
                }
                WsEvent::ListUpdate { list_id, action } => {
                    // Check if this update affects the currently viewed list
                    let current_list_id = self.selected_list().map(|l| l.id.clone());
                    
                    if current_list_id.as_ref() == Some(&list_id) && self.view == View::Items {
                        should_refresh_items = true;
                        last_action = action.clone();
                    }
                    
                    // For list-level changes, refresh the list
                    if action.starts_with("list-") {
                        should_refresh_lists = true;
                    }
                }
            }
        }
        
        // Perform refreshes after processing all events
        if should_refresh_lists {
            self.refresh_lists();
        }
        if should_refresh_items {
            self.load_items();
            self.status_message = Some(format!("↻ {}", last_action));
        }
    }
}

impl Model for ListsTableBrowser {
    type Message = Msg;
    
    fn init(&mut self) -> Cmd<Msg> {
        // Set tick rate for polling WebSocket events
        Cmd::tick(Duration::from_millis(200))
    }
    
    fn update(&mut self, msg: Msg) -> Cmd<Msg> {
        // Poll WebSocket events on every update
        self.poll_ws_events();
        
        match msg {
            Msg::Event(event) => {
                // Handle delete confirmation dialog first
                if self.delete_confirm.is_some() && self.dialog_state.is_open() {
                    let dialog = Dialog::confirm("Delete Item?", "Are you sure you want to delete this item?");
                    if let Some(result) = dialog.handle_event(&event, &mut self.dialog_state, None) {
                        match result {
                            DialogResult::Ok => {
                                self.do_delete_confirmed();
                            }
                            DialogResult::Cancel | DialogResult::Dismissed => {
                                self.delete_confirm = None;
                            }
                            _ => {}
                        }
                        return Cmd::none();
                    }
                    return Cmd::none();
                }
                
                // Handle mouse events
                if let Event::Mouse(mouse) = &event {
                    match mouse.kind {
                        MouseEventKind::Down(MouseButton::Left) => {
                            let sidebar = self.sidebar_area.get();
                            let table = self.table_area.get();
                            
                            if sidebar.contains(mouse.x, mouse.y) {
                                self.focus = Focus::Sidebar;
                                let rel_y = mouse.y.saturating_sub(sidebar.y) as usize;
                                let filtered_len = self.filtered_lists().len();
                                if rel_y < filtered_len {
                                    self.list_index = rel_y;
                                    self.load_items();  // Single click opens list
                                }
                            } else if table.contains(mouse.x, mouse.y) {
                                self.focus = Focus::Table;
                                // Table row click - account for header row
                                let rel_y = mouse.y.saturating_sub(table.y) as usize;
                                if rel_y == 0 {
                                    // Header row clicked - sort by column
                                    if self.view == View::Items || self.view == View::AqlQuery {
                                        let default_columns = vec![Column::Title, Column::Tags];
                                        let columns: Vec<Column> = if self.view == View::AqlQuery {
                                            self.aql_columns.clone().unwrap_or_else(|| default_columns.clone())
                                        } else {
                                            default_columns
                                        };
                                        if let Some(col) = self.get_column_at_x(mouse.x, &columns) {
                                            self.sort_by_column(col);
                                        }
                                    }
                                } else {
                                    // Data row clicked
                                    let row_idx = rel_y - 1;
                                    let items = self.current_items();
                                    if row_idx < items.len() {
                                        let current_selected = self.current_table_state().selected;
                                        if current_selected == Some(row_idx) {
                                            self.show_detail();
                                        } else {
                                            self.current_table_state_mut().select(Some(row_idx));
                                        }
                                    }
                                }
                            }
                        }
                        MouseEventKind::ScrollUp => {
                            if self.focus == Focus::Sidebar {
                                if self.list_index > 0 {
                                    self.list_index -= 1;
                                }
                            } else {
                                self.move_up();
                            }
                        }
                        MouseEventKind::ScrollDown => {
                            if self.focus == Focus::Sidebar {
                                if self.list_index + 1 < self.filtered_lists().len() {
                                    self.list_index += 1;
                                }
                            } else {
                                self.move_down();
                            }
                        }
                        _ => {}
                    }
                    return Cmd::none();
                }
                
                // Handle keyboard events
                if let Event::Key(key_event) = event {
                    match self.input_mode {
                        InputMode::Search => {
                            match key_event.code {
                                KeyCode::Char(c) if !key_event.modifiers.contains(Modifiers::CTRL) => {
                                    self.search_query.push(c);
                                    self.do_search();  // Real-time search
                                }
                                _ => return self.handle_key(key_event.code, key_event.modifiers),
                            }
                        }
                        InputMode::AddItem => {
                            match key_event.code {
                                KeyCode::Char(c) if !key_event.modifiers.contains(Modifiers::CTRL) => {
                                    self.add_item_text.push(c);
                                }
                                _ => return self.handle_key(key_event.code, key_event.modifiers),
                            }
                        }
                        InputMode::AqlQuery => {
                            match key_event.code {
                                KeyCode::Char(c) if !key_event.modifiers.contains(Modifiers::CTRL) => {
                                    self.aql_query.push(c);
                                }
                                _ => return self.handle_key(key_event.code, key_event.modifiers),
                            }
                        }
                        InputMode::ListFilter => {
                            match key_event.code {
                                KeyCode::Char(c) if !key_event.modifiers.contains(Modifiers::CTRL) => {
                                    self.list_filter.push(c);
                                }
                                _ => return self.handle_key(key_event.code, key_event.modifiers),
                            }
                        }
                        InputMode::Normal => {
                            return self.handle_key(key_event.code, key_event.modifiers);
                        }
                    }
                }
            }
        }
        Cmd::none()
    }
    
    fn view(&self, frame: &mut Frame) {
        let area = Rect::from_size(frame.buffer.width(), frame.buffer.height());
        
        // Fill background
        frame.buffer.fill(area, RenderCell::default().with_bg(colors::BG_PRIMARY));
        
        // Calculate sidebar width based on longest list name
        let filtered = self.filtered_lists();
        let max_name_len = filtered.iter()
            .map(|l| l.name.chars().count())
            .max()
            .unwrap_or(10);
        // Add space for: border (2) + prefix "▶ " (2) + padding (2)
        let sidebar_width = (max_name_len + 6).min(40).max(15) as u16;
        
        // Main layout: sidebar | table
        let chunks = Flex::horizontal()
            .constraints([
                Constraint::Fixed(sidebar_width),
                Constraint::Min(40),    // Table
            ])
            .split(Rect::new(0, 0, area.width, area.height.saturating_sub(2)));
        
        // === Sidebar (Lists) ===
        let sidebar_style = if self.focus == Focus::Sidebar {
            Style::new().fg(colors::BORDER_FOCUSED)
        } else {
            Style::new().fg(colors::BORDER_UNFOCUSED)
        };
        
        let sidebar_title = if self.list_filter.is_empty() {
            format!(" Lists ({}) ", filtered.len())
        } else {
            format!(" /{} ({}) ", self.list_filter, filtered.len())
        };
        
        let sidebar_block = Block::default()
            .title(&sidebar_title)
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(sidebar_style);
        
        let sidebar_inner = sidebar_block.inner(chunks[0]);
        self.sidebar_area.set(sidebar_inner);
        sidebar_block.render(chunks[0], frame);
        
        // Render list names (filtered)
        for (i, list) in filtered.iter().enumerate() {
            if i >= sidebar_inner.height as usize {
                break;
            }
            
            let y = sidebar_inner.y + i as u16;
            let is_selected = i == self.list_index;
            
            let style = if is_selected {
                Style::new().fg(colors::FG_PRIMARY).bg(colors::BG_SELECTED)
            } else {
                Style::new().fg(colors::FG_SECONDARY)
            };
            
            let prefix = if is_selected { "▶ " } else { "  " };
            let text = format!("{}{}", prefix, list.name);
            let truncated: String = text.chars().take(sidebar_inner.width as usize).collect();
            
            Paragraph::new(truncated)
                .style(style)
                .render(Rect::new(sidebar_inner.x, y, sidebar_inner.width, 1), frame);
        }
        
        // === Main Panel (Table) ===
        let table_style = if self.focus == Focus::Table {
            Style::new().fg(colors::BORDER_FOCUSED)
        } else {
            Style::new().fg(colors::BORDER_UNFOCUSED)
        };
        
        let table_title = match self.view {
            View::Lists => " Select a list ".to_string(),
            View::Items => {
                if let Some(list) = self.selected_list() {
                    format!(" {} ({} items) ", list.name, self.items.len())
                } else {
                    " Items ".to_string()
                }
            }
            View::Search => format!(" Search: \"{}\" ({} results) ", self.search_query, self.search_results.len()),
            View::AqlQuery => {
                if let Some(list) = self.selected_list() {
                    format!(" AQL [{}]: {} ({} results) ", list.name, self.aql_query, self.aql_results.len())
                } else {
                    format!(" AQL: {} ({} results) ", self.aql_query, self.aql_results.len())
                }
            }
            View::Detail => {
                if let Some(ref item) = self.detail_item {
                    format!(" {} ", item.title)
                } else {
                    " Detail ".to_string()
                }
            }
        };
        
        let table_block = Block::default()
            .title(&table_title)
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(table_style);
        
        let table_inner = table_block.inner(chunks[1]);
        self.table_area.set(table_inner);
        table_block.render(chunks[1], frame);
        
        // Render content based on view
        match self.view {
            View::Lists => {
                let msg = "Select a list from the sidebar and press Enter";
                Paragraph::new(msg)
                    .style(Style::new().fg(colors::FG_MUTED))
                    .render(table_inner, frame);
            }
            View::Items | View::Search | View::AqlQuery => {
                self.render_items_table(frame, table_inner);
            }
            View::Detail => {
                self.render_detail(frame, table_inner);
            }
        }
        
        // === Input bar ===
        let input_area = Rect::new(0, area.height.saturating_sub(2), area.width, 1);
        
        match self.input_mode {
            InputMode::Search => {
                let text = format!("Search: {}▌", self.search_query);
                Paragraph::new(text)
                    .style(Style::new().fg(colors::ACCENT_PRIMARY).bg(colors::BG_SURFACE))
                    .render(input_area, frame);
            }
            InputMode::AddItem => {
                let text = format!("New item: {}▌", self.add_item_text);
                Paragraph::new(text)
                    .style(Style::new().fg(colors::FG_SUCCESS).bg(colors::BG_SURFACE))
                    .render(input_area, frame);
            }
            InputMode::AqlQuery => {
                let text = format!("AQL: {}▌", self.aql_query);
                Paragraph::new(text)
                    .style(Style::new().fg(colors::ACCENT_TAG).bg(colors::BG_SURFACE))
                    .render(input_area, frame);
            }
            InputMode::ListFilter => {
                let text = format!("Filter lists: {}▌", self.list_filter);
                Paragraph::new(text)
                    .style(Style::new().fg(colors::ACCENT_PRIMARY).bg(colors::BG_SURFACE))
                    .render(input_area, frame);
            }
            InputMode::Normal => {
                if let Some(ref msg) = self.error_message {
                    Paragraph::new(msg.as_str())
                        .style(Style::new().fg(colors::FG_ERROR).bg(colors::BG_SURFACE))
                        .render(input_area, frame);
                } else if let Some(ref msg) = self.status_message {
                    Paragraph::new(msg.as_str())
                        .style(Style::new().fg(colors::FG_SUCCESS).bg(colors::BG_SURFACE))
                        .render(input_area, frame);
                }
            }
        }
        
        // === Status bar ===
        let status_area = Rect::new(0, area.height.saturating_sub(1), area.width, 1);
        let ws_indicator = if self.ws_connected { "🔗" } else { "⚡" };
        let status = match self.input_mode {
            InputMode::Search => format!("{} Enter:search  Esc:cancel ", ws_indicator),
            InputMode::AddItem => format!("{} Enter:add  Esc:cancel ", ws_indicator),
            InputMode::AqlQuery => format!("{} Enter:run-query  Esc:cancel ", ws_indicator),
            InputMode::ListFilter => format!("{} Enter:select  Esc:clear-filter ", ws_indicator),
            InputMode::Normal => match self.view {
                View::Detail => format!("{} Esc:back  o:open-url  j/k:scroll  q:quit ", ws_indicator),
                _ => format!("{} q:quit  /:search  ::aql  n:new  d:del  Space:toggle  o:open  y:copy ", ws_indicator),
            },
        };
        
        Paragraph::new(status)
            .style(Style::new().fg(colors::FG_MUTED).bg(colors::BG_SURFACE))
            .render(status_area, frame);
        
        // === Delete Confirmation Dialog ===
        if let Some((_, _, ref title)) = self.delete_confirm {
            if self.dialog_state.is_open() {
                let message = format!("Delete \"{}\"?", title);
                let dialog = Dialog::confirm("Confirm Delete", &message);
                let mut state = self.dialog_state.clone();
                StatefulWidget::render(&dialog, area, frame, &mut state);
            }
        }
    }
}

impl ListsTableBrowser {
    /// Get lists filtered by the current list_filter
    fn filtered_lists(&self) -> Vec<&ListInfo> {
        if self.list_filter.is_empty() {
            self.lists.iter().collect()
        } else {
            let filter_lower = self.list_filter.to_lowercase();
            self.lists
                .iter()
                .filter(|l| l.name.to_lowercase().contains(&filter_lower))
                .collect()
        }
    }
    
    /// Get the actual list at the current index (accounting for filter)
    fn selected_list(&self) -> Option<&ListInfo> {
        self.filtered_lists().get(self.list_index).copied()
    }
    
    fn refresh_lists(&mut self) {
        // LISTS_FILTER env var: comma-separated substrings to match (case-insensitive)
        // If not set or empty, shows all lists
        let filter: Vec<String> = env::var("LISTS_FILTER")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty())
            .collect();
        
        match fetch_lists(&self.base_url) {
            Ok(lists) => {
                self.lists = if filter.is_empty() {
                    lists
                } else {
                    lists
                        .into_iter()
                        .filter(|l| {
                            let name_lower = l.name.to_lowercase();
                            let id_lower = l.id.to_lowercase();
                            filter.iter().any(|f| name_lower.contains(f) || id_lower.contains(f))
                        })
                        .collect()
                };
                self.error_message = None;
                self.status_message = Some(format!("Loaded {} lists", self.lists.len()));
            }
            Err(e) => {
                self.error_message = Some(format!("Failed to load lists: {}", e));
            }
        }
    }
    
    /// Sort items with completed at bottom
    fn sort_items(items: &mut Vec<ListItem>) {
        items.sort_by(|a, b| {
            let a_completed = a.completed.unwrap_or(false);
            let b_completed = b.completed.unwrap_or(false);
            a_completed.cmp(&b_completed) // false < true, so incomplete first
        });
    }
    
    fn load_items(&mut self) {
        if let Some(list) = self.selected_list() {
            match fetch_items(&self.base_url, &list.id, 100) {
                Ok(mut items) => {
                    Self::sort_items(&mut items);
                    self.items = items;
                    self.table_state.select(if self.items.is_empty() { None } else { Some(0) });
                    self.view = View::Items;
                    self.focus = Focus::Table;
                    self.error_message = None;
                    self.status_message = Some(format!("Loaded {} items", self.items.len()));
                }
                Err(e) => {
                    self.error_message = Some(format!("Failed to load items: {}", e));
                }
            }
        }
    }
    
    fn do_search(&mut self) {
        if self.search_query.is_empty() {
            return;
        }
        
        // If a list is selected, constrain search to that list
        let list_id = self.selected_list().map(|l| l.id.clone());
        
        match search_items(&self.base_url, &self.search_query, list_id.as_deref(), 50) {
            Ok(mut results) => {
                Self::sort_items(&mut results);
                self.search_results = results;
                self.search_table_state.select(if self.search_results.is_empty() { None } else { Some(0) });
                self.view = View::Search;
                self.focus = Focus::Table;
                self.error_message = None;
                let scope = if list_id.is_some() { "in list" } else { "all lists" };
                self.status_message = Some(format!("Found {} results ({})", self.search_results.len(), scope));
            }
            Err(e) => {
                self.error_message = Some(format!("Search failed: {}", e));
            }
        }
    }
    
    fn do_aql_query(&mut self) {
        if self.aql_query.is_empty() {
            return;
        }
        
        // AQL query requires a list to be selected
        let list_id = match self.selected_list() {
            Some(list) => list.id.clone(),
            None => {
                self.error_message = Some("Select a list first".to_string());
                return;
            }
        };
        
        // Parse `show` clause from query (e.g., "... show title, tags, notes")
        self.aql_columns = Self::parse_show_clause(&self.aql_query);
        
        match aql_query(&self.base_url, &list_id, &self.aql_query, 100) {
            Ok(mut results) => {
                Self::sort_items(&mut results);
                self.aql_results = results;
                self.aql_table_state.select(if self.aql_results.is_empty() { None } else { Some(0) });
                self.view = View::AqlQuery;
                self.focus = Focus::Table;
                self.error_message = None;
                self.status_message = Some(format!("AQL: {} results", self.aql_results.len()));
            }
            Err(e) => {
                self.error_message = Some(format!("AQL error: {}", e));
            }
        }
    }
    
    fn parse_show_clause(query: &str) -> Option<Vec<Column>> {
        // Find "show" keyword (case insensitive)
        // Can be at start of query or after a space
        let lower = query.to_lowercase();
        let (show_idx, skip_len) = if lower.starts_with("show ") {
            (0, 5)
        } else if let Some(idx) = lower.find(" show ") {
            (idx, 6)
        } else {
            return None;
        };
        
        // Extract everything after "show" until "order by" or end
        let after_show = &query[show_idx + skip_len..];
        let columns_str = if let Some(order_idx) = after_show.to_lowercase().find(" order ") {
            &after_show[..order_idx]
        } else {
            after_show
        };
        
        // Parse comma-separated column names
        let columns: Vec<Column> = columns_str
            .split(',')
            .filter_map(|s| Column::from_str(s.trim()))
            .collect();
        
        if columns.is_empty() {
            None
        } else {
            Some(columns)
        }
    }
    
    fn do_add_item(&mut self) {
        if self.add_item_text.is_empty() {
            return;
        }
        
        if let Some(list) = self.selected_list() {
            match add_item(&self.base_url, &list.id, &self.add_item_text) {
                Ok(item) => {
                    self.items.insert(0, item);
                    self.table_state.select(Some(0));
                    self.status_message = Some("Item added".to_string());
                    self.error_message = None;
                }
                Err(e) => {
                    self.error_message = Some(format!("Failed to add item: {}", e));
                }
            }
        }
        
        self.add_item_text.clear();
    }
    
    fn delete_selected_item(&mut self) {
        // Show confirmation dialog instead of deleting immediately
        let (items, state) = match self.view {
            View::Items => (&self.items, &self.table_state),
            View::Search => (&self.search_results, &self.search_table_state),
            View::AqlQuery => (&self.aql_results, &self.aql_table_state),
            _ => return,
        };
        
        if let Some(idx) = state.selected {
            if let Some(item) = items.get(idx) {
                let list_id = self.selected_list().map(|l| l.id.clone());
                if let Some(list_id) = list_id {
                    // Store item info and show dialog
                    self.delete_confirm = Some((list_id, item.id.clone(), item.title.clone()));
                    self.dialog_state = DialogState::new();
                }
            }
        }
    }
    
    fn do_delete_confirmed(&mut self) {
        let Some((list_id, item_id, _)) = self.delete_confirm.take() else {
            return;
        };
        
        match delete_item(&self.base_url, &list_id, &item_id) {
            Ok(()) => {
                // Remove from the appropriate list
                let (items, state) = match self.view {
                    View::Items => (&mut self.items, &mut self.table_state),
                    View::Search => (&mut self.search_results, &mut self.search_table_state),
                    View::AqlQuery => (&mut self.aql_results, &mut self.aql_table_state),
                    _ => return,
                };
                
                if let Some(idx) = items.iter().position(|i| i.id == item_id) {
                    items.remove(idx);
                    if let Some(selected) = state.selected {
                        if selected >= items.len() && !items.is_empty() {
                            state.select(Some(items.len() - 1));
                        } else if items.is_empty() {
                            state.select(None);
                        }
                    }
                }
                self.status_message = Some("Item deleted".to_string());
                self.error_message = None;
            }
            Err(e) => {
                self.error_message = Some(format!("Failed to delete: {}", e));
            }
        }
    }
    
    fn toggle_completed(&mut self) {
        // Get list ID first to avoid borrow issues
        let Some(list_id) = self.selected_list().map(|l| l.id.clone()) else { return };
        
        let (items, state) = match self.view {
            View::Items => (&mut self.items, &self.table_state),
            View::Search => (&mut self.search_results, &self.search_table_state),
            View::AqlQuery => (&mut self.aql_results, &self.aql_table_state),
            _ => return,
        };
        
        let Some(idx) = state.selected else { return };
        let Some(item) = items.get(idx) else { return };
        
        let new_completed = !item.completed.unwrap_or(false);
        let item_id = item.id.clone();
        
        match update_item_completed(&self.base_url, &list_id, &item_id, new_completed) {
            Ok(updated_item) => {
                // Update local item
                if let Some(item) = items.get_mut(idx) {
                    item.completed = updated_item.completed;
                }
                // Re-sort to move completed items to bottom
                Self::sort_items(items);
                self.status_message = Some(if new_completed { "✓ Completed" } else { "○ Uncompleted" }.to_string());
                self.error_message = None;
            }
            Err(e) => {
                self.error_message = Some(format!("Failed to update: {}", e));
            }
        }
    }
    
    fn move_to_top(&mut self) {
        // Only works in Items view (not search/aql which are cross-list)
        if self.view != View::Items {
            self.status_message = Some("Move only works in list view".to_string());
            return;
        }
        
        let Some(idx) = self.table_state.selected else { return };
        let Some(item) = self.items.get(idx) else { return };
        let Some(list) = self.selected_list() else { return };
        
        match update_item_position(&self.base_url, &list.id, &item.id, 0) {
            Ok(_) => {
                self.status_message = Some("↑ Moved to top".to_string());
                self.error_message = None;
                // Reload to get updated order
                self.load_items();
            }
            Err(e) => {
                self.error_message = Some(format!("Failed to move: {}", e));
            }
        }
    }
    
    fn move_to_bottom(&mut self) {
        // Only works in Items view (not search/aql which are cross-list)
        if self.view != View::Items {
            self.status_message = Some("Move only works in list view".to_string());
            return;
        }
        
        let Some(idx) = self.table_state.selected else { return };
        let Some(item) = self.items.get(idx) else { return };
        let Some(list) = self.selected_list() else { return };
        
        // Use a large number - API will clamp to end
        let last_pos = self.items.len().saturating_sub(1);
        
        match update_item_position(&self.base_url, &list.id, &item.id, last_pos) {
            Ok(_) => {
                self.status_message = Some("↓ Moved to bottom".to_string());
                self.error_message = None;
                // Reload to get updated order
                self.load_items();
            }
            Err(e) => {
                self.error_message = Some(format!("Failed to move: {}", e));
            }
        }
    }
    
    fn sort_by_column(&mut self, column: Column) {
        // Toggle direction if same column, otherwise default to desc
        if self.sort_column == Some(column) {
            self.sort_direction = self.sort_direction.toggle();
        } else {
            self.sort_column = Some(column);
            self.sort_direction = SortDirection::Desc;
        }
        
        let order_clause = format!("order by {} {}", column.aql_field(), self.sort_direction.aql_suffix());
        
        // Update or create AQL query
        if self.aql_query.is_empty() {
            // No existing query - just set order by
            self.aql_query = order_clause;
        } else {
            // Update existing query - replace or append order by
            let lower = self.aql_query.to_lowercase();
            if let Some(order_idx) = lower.find(" order by ") {
                // Replace existing order by clause
                self.aql_query = format!("{} {}", &self.aql_query[..order_idx], order_clause);
            } else if lower.starts_with("order by ") {
                // Query is just an order by - replace it
                self.aql_query = order_clause;
            } else {
                // Append order by to existing query
                self.aql_query = format!("{} {}", self.aql_query, order_clause);
            }
        }
        
        // Run the query
        self.do_aql_query();
    }
    
    fn get_column_at_x(&self, x: u16, _columns: &[Column]) -> Option<Column> {
        let positions = self.column_positions.take();
        let result = positions.iter()
            .find(|(_, start, end)| x >= *start && x < *end)
            .map(|(col, _, _)| *col);
        self.column_positions.set(positions);
        result
    }
    
    fn show_detail(&mut self) {
        let item = match self.view {
            View::Items => self.table_state.selected.and_then(|i| self.items.get(i).cloned()),
            View::Search => self.search_table_state.selected.and_then(|i| self.search_results.get(i).cloned()),
            View::AqlQuery => self.aql_table_state.selected.and_then(|i| self.aql_results.get(i).cloned()),
            _ => None,
        };
        
        if let Some(item) = item {
            self.detail_item = Some(item);
            self.detail_scroll = 0;
            self.view = View::Detail;
        }
    }
    
    fn open_selected_url(&self) {
        let url = match self.view {
            View::Items => self.table_state.selected.and_then(|i| self.items.get(i).and_then(|item| item.url.clone())),
            View::Search => self.search_table_state.selected.and_then(|i| self.search_results.get(i).and_then(|item| item.url.clone())),
            View::AqlQuery => self.aql_table_state.selected.and_then(|i| self.aql_results.get(i).and_then(|item| item.url.clone())),
            View::Detail => self.detail_item.as_ref().and_then(|i| i.url.clone()),
            _ => None,
        };
        
        if let Some(url) = url {
            let _ = Command::new("xdg-open")
                .arg(&url)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
        }
    }
    
    fn copy_selected(&mut self) {
        // Get the selected item
        let item = match self.view {
            View::Items => self.table_state.selected.and_then(|i| self.items.get(i)),
            View::Search => self.search_table_state.selected.and_then(|i| self.search_results.get(i)),
            View::AqlQuery => self.aql_table_state.selected.and_then(|i| self.aql_results.get(i)),
            View::Detail => self.detail_item.as_ref(),
            _ => None,
        };
        
        let Some(item) = item else {
            return;
        };
        
        // Build copy text: URL if available, otherwise title
        let text = item.url.as_deref().unwrap_or(&item.title);
        
        // Write OSC 52 directly to stdout (works over SSH)
        // Format: ESC ] 52 ; c ; <base64> BEL
        use base64::{Engine as _, engine::general_purpose::STANDARD};
        let encoded = STANDARD.encode(text.as_bytes());
        let osc52 = format!("\x1b]52;c;{}\x07", encoded);
        
        let mut stdout = std::io::stdout();
        if stdout.write_all(osc52.as_bytes()).is_ok() && stdout.flush().is_ok() {
            self.status_message = Some(format!("Copied: {}", text.chars().take(40).collect::<String>()));
            self.error_message = None;
        } else {
            self.error_message = Some("Copy failed: write error".to_string());
        }
    }
    
    fn current_items(&self) -> &[ListItem] {
        match self.view {
            View::Items => &self.items,
            View::Search => &self.search_results,
            View::AqlQuery => &self.aql_results,
            _ => &[],
        }
    }
    
    fn current_table_state(&self) -> &TableState {
        match self.view {
            View::Search => &self.search_table_state,
            View::AqlQuery => &self.aql_table_state,
            _ => &self.table_state,
        }
    }
    
    fn current_table_state_mut(&mut self) -> &mut TableState {
        match self.view {
            View::Search => &mut self.search_table_state,
            View::AqlQuery => &mut self.aql_table_state,
            _ => &mut self.table_state,
        }
    }
    
    fn move_up(&mut self) {
        if self.view == View::Detail {
            if self.detail_scroll > 0 {
                self.detail_scroll -= 1;
            }
            return;
        }
        
        let state = self.current_table_state_mut();
        if let Some(selected) = state.selected {
            if selected > 0 {
                state.select(Some(selected - 1));
            }
        }
    }
    
    fn move_down(&mut self) {
        if self.view == View::Detail {
            self.detail_scroll += 1;
            return;
        }
        
        let items_len = self.current_items().len();
        let state = self.current_table_state_mut();
        if let Some(selected) = state.selected {
            if selected + 1 < items_len {
                state.select(Some(selected + 1));
            }
        }
    }
    
    fn handle_key(&mut self, code: KeyCode, modifiers: Modifiers) -> Cmd<Msg> {
        match self.input_mode {
            InputMode::Search => {
                match code {
                    KeyCode::Escape => {
                        self.input_mode = InputMode::Normal;
                        self.search_query.clear();
                    }
                    KeyCode::Char('c') if modifiers.contains(Modifiers::CTRL) => {
                        if self.search_query.is_empty() {
                            self.input_mode = InputMode::Normal;
                        } else {
                            self.search_query.clear();
                            self.search_results.clear();
                        }
                    }
                    KeyCode::Backspace => {
                        self.search_query.pop();
                        if self.search_query.is_empty() {
                            self.search_results.clear();
                        } else {
                            self.do_search();  // Real-time search
                        }
                    }
                    KeyCode::Enter => {
                        self.input_mode = InputMode::Normal;
                        // Keep results visible, just exit input mode
                    }
                    _ => {}
                }
            }
            InputMode::AddItem => {
                match code {
                    KeyCode::Escape => {
                        self.input_mode = InputMode::Normal;
                        self.add_item_text.clear();
                    }
                    KeyCode::Char('c') if modifiers.contains(Modifiers::CTRL) => {
                        if self.add_item_text.is_empty() {
                            self.input_mode = InputMode::Normal;
                        } else {
                            self.add_item_text.clear();
                        }
                    }
                    KeyCode::Backspace => {
                        self.add_item_text.pop();
                    }
                    KeyCode::Enter => {
                        self.input_mode = InputMode::Normal;
                        self.do_add_item();
                    }
                    _ => {}
                }
            }
            InputMode::AqlQuery => {
                match code {
                    KeyCode::Escape => {
                        self.input_mode = InputMode::Normal;
                        self.aql_query.clear();
                    }
                    KeyCode::Char('c') if modifiers.contains(Modifiers::CTRL) => {
                        if self.aql_query.is_empty() {
                            self.input_mode = InputMode::Normal;
                        } else {
                            self.aql_query.clear();
                        }
                    }
                    KeyCode::Backspace => {
                        self.aql_query.pop();
                    }
                    KeyCode::Enter => {
                        self.input_mode = InputMode::Normal;
                        self.do_aql_query();
                    }
                    _ => {}
                }
            }
            InputMode::ListFilter => {
                match code {
                    KeyCode::Escape => {
                        self.input_mode = InputMode::Normal;
                        self.list_filter.clear();
                        self.list_index = 0;
                    }
                    KeyCode::Char('c') if modifiers.contains(Modifiers::CTRL) => {
                        if self.list_filter.is_empty() {
                            self.input_mode = InputMode::Normal;
                        } else {
                            self.list_filter.clear();
                            self.list_index = 0;
                        }
                    }
                    KeyCode::Backspace => {
                        self.list_filter.pop();
                        self.list_index = 0;  // Reset selection when filter changes
                    }
                    KeyCode::Enter => {
                        self.input_mode = InputMode::Normal;
                        // Keep filter active, load selected list
                        if !self.filtered_lists().is_empty() {
                            self.load_items();
                        }
                    }
                    KeyCode::Up => {
                        if self.list_index > 0 {
                            self.list_index -= 1;
                        }
                    }
                    KeyCode::Down => {
                        if self.list_index + 1 < self.filtered_lists().len() {
                            self.list_index += 1;
                        }
                    }
                    _ => {}
                }
            }
            InputMode::Normal => {
                match code {
                    KeyCode::Char('q') => return Cmd::quit(),
                    KeyCode::Char('c') if modifiers.contains(Modifiers::CTRL) => return Cmd::quit(),
                    
                    KeyCode::Char('/') => {
                        if self.focus == Focus::Sidebar {
                            self.input_mode = InputMode::ListFilter;
                            // Keep existing filter
                        } else {
                            self.input_mode = InputMode::Search;
                            self.search_query.clear();
                        }
                    }
                    
                    KeyCode::Char(':') => {
                        self.input_mode = InputMode::AqlQuery;
                        // Keep existing query if there is one
                    }
                    
                    KeyCode::Tab => {
                        self.focus = match self.focus {
                            Focus::Sidebar => Focus::Table,
                            Focus::Table => Focus::Sidebar,
                        };
                    }
                    
                    KeyCode::Escape | KeyCode::Backspace => {
                        // Sidebar focus: clear list filter if active
                        if self.focus == Focus::Sidebar && !self.list_filter.is_empty() {
                            self.list_filter.clear();
                            self.list_index = 0;
                        }
                        // Table focus: clear selection first, then go back
                        else if self.focus == Focus::Table {
                            let has_selection = match self.view {
                                View::Items => self.table_state.selected.is_some(),
                                View::Search => self.search_table_state.selected.is_some(),
                                View::AqlQuery => self.aql_table_state.selected.is_some(),
                                _ => false,
                            };
                            
                            if has_selection {
                                // Clear selection
                                match self.view {
                                    View::Items => self.table_state.select(None),
                                    View::Search => self.search_table_state.select(None),
                                    View::AqlQuery => self.aql_table_state.select(None),
                                    _ => {}
                                }
                            } else {
                                // No selection, go back
                                match self.view {
                                    View::Detail => {
                                        self.view = if !self.search_results.is_empty() && !self.search_query.is_empty() {
                                            View::Search
                                        } else if !self.aql_results.is_empty() && !self.aql_query.is_empty() {
                                            View::AqlQuery
                                        } else {
                                            View::Items
                                        };
                                        self.detail_item = None;
                                    }
                                    View::Items | View::Search | View::AqlQuery => {
                                        self.view = View::Lists;
                                        self.focus = Focus::Sidebar;
                                    }
                                    View::Lists => {}
                                }
                            }
                        }
                        // Other cases (detail view)
                        else {
                            match self.view {
                                View::Detail => {
                                    self.view = if !self.search_results.is_empty() && !self.search_query.is_empty() {
                                        View::Search
                                    } else if !self.aql_results.is_empty() && !self.aql_query.is_empty() {
                                        View::AqlQuery
                                    } else {
                                        View::Items
                                    };
                                    self.detail_item = None;
                                }
                                _ => {}
                            }
                        }
                    }
                    
                    KeyCode::Up | KeyCode::Char('k') => {
                        if self.focus == Focus::Sidebar {
                            if self.list_index > 0 {
                                self.list_index -= 1;
                            }
                        } else {
                            self.move_up();
                        }
                    }
                    
                    KeyCode::Down | KeyCode::Char('j') => {
                        if self.focus == Focus::Sidebar {
                            if self.list_index + 1 < self.filtered_lists().len() {
                                self.list_index += 1;
                            }
                        } else {
                            self.move_down();
                        }
                    }
                    
                    KeyCode::Enter => {
                        if self.focus == Focus::Sidebar {
                            self.load_items();
                        } else if self.view == View::Items || self.view == View::Search {
                            self.show_detail();
                        }
                    }
                    
                    KeyCode::Char('o') => {
                        self.open_selected_url();
                    }
                    
                    KeyCode::Char('y') => {
                        self.copy_selected();
                    }
                    
                    KeyCode::Char('n') => {
                        if self.view == View::Items {
                            self.input_mode = InputMode::AddItem;
                            self.add_item_text.clear();
                        }
                    }
                    
                    KeyCode::Char('d') => {
                        if self.view == View::Items || self.view == View::Search || self.view == View::AqlQuery {
                            self.delete_selected_item();
                        }
                    }
                    
                    KeyCode::Char(' ') => {
                        if self.view == View::Items || self.view == View::Search || self.view == View::AqlQuery {
                            self.toggle_completed();
                        }
                    }
                    
                    KeyCode::Char('t') => {
                        self.move_to_top();
                    }
                    
                    KeyCode::Char('b') => {
                        self.move_to_bottom();
                    }
                    
                    KeyCode::Char('r') => {
                        self.refresh_lists();
                        if self.view == View::Items {
                            self.load_items();
                        }
                    }
                    
                    _ => {}
                }
            }
        }
        Cmd::none()
    }
    
    fn render_items_table(&self, frame: &mut Frame, area: Rect) {
        let items = self.current_items();
        
        if items.is_empty() {
            Paragraph::new("No items")
                .style(Style::new().fg(colors::FG_MUTED))
                .render(area, frame);
            return;
        }
        
        // Determine which columns to show (default: title and tags)
        let default_columns = vec![Column::Title, Column::Tags];
        let columns: Vec<Column> = if self.view == View::AqlQuery {
            self.aql_columns.clone().unwrap_or_else(|| default_columns.clone())
        } else {
            default_columns
        };
        
        // Build table rows based on columns
        let rows: Vec<Row> = items.iter().map(|item| {
            let is_completed = item.completed.unwrap_or(false);
            let cells: Vec<String> = columns.iter().map(|col| {
                match col {
                    Column::Status => {
                        if is_completed { "✓".to_string() } else { "○".to_string() }
                    }
                    Column::Title => {
                        if is_completed {
                            format!("̶{}", item.title) // Unicode combining long stroke overlay
                        } else {
                            item.title.clone()
                        }
                    }
                    Column::Tags => {
                        if item.tags.is_empty() {
                            String::new()
                        } else {
                            item.tags.iter().take(3).cloned().collect::<Vec<_>>().join(", ")
                        }
                    }
                    Column::Url => {
                        if item.url.is_some() { "🔗".to_string() } else { "  ".to_string() }
                    }
                    Column::Notes => {
                        // Show truncated notes content instead of just icon
                        if let Some(ref notes) = item.notes {
                            let first_line = notes.lines().next().unwrap_or("");
                            if first_line.len() > 40 {
                                format!("{}...", &first_line[..37])
                            } else {
                                first_line.to_string()
                            }
                        } else {
                            String::new()
                        }
                    }
                }
            }).collect();
            
            let row = Row::new(cells);
            if is_completed {
                row.style(Style::new().fg(colors::FG_MUTED).strikethrough())
            } else {
                row
            }
        }).collect();
        
        // Column widths based on which columns are shown
        let widths: Vec<Constraint> = columns.iter().map(|col| {
            match col {
                Column::Status => Constraint::Fixed(2),
                Column::Title => Constraint::Min(20),
                Column::Tags => Constraint::Fixed(20),
                Column::Url => Constraint::Fixed(3),
                Column::Notes => Constraint::Min(30),
            }
        }).collect();
        
        // Calculate actual column positions for click detection
        // Account for column_spacing(1) between columns
        let spacing = 1u16;
        let total_spacing = spacing * (columns.len().saturating_sub(1)) as u16;
        let available = area.width.saturating_sub(total_spacing);
        
        // Calculate widths similar to how Layout would
        let mut col_widths: Vec<u16> = Vec::new();
        let mut remaining = available;
        let mut min_cols: Vec<usize> = Vec::new();
        
        for (i, col) in columns.iter().enumerate() {
            match col {
                Column::Status => col_widths.push(2),
                Column::Title => { col_widths.push(20); min_cols.push(i); }
                Column::Tags => col_widths.push(20),
                Column::Url => col_widths.push(3),
                Column::Notes => { col_widths.push(30); min_cols.push(i); }
            }
        }
        
        // Subtract fixed widths
        for (i, w) in col_widths.iter().enumerate() {
            if !min_cols.contains(&i) {
                remaining = remaining.saturating_sub(*w);
            }
        }
        
        // Distribute remaining space to Min columns
        if !min_cols.is_empty() {
            let min_total: u16 = min_cols.iter().map(|&i| col_widths[i]).sum();
            remaining = remaining.saturating_sub(min_total);
            let extra_per = remaining / min_cols.len() as u16;
            for &i in &min_cols {
                col_widths[i] += extra_per;
            }
        }
        
        // Build column positions
        let mut positions: Vec<(Column, u16, u16)> = Vec::new();
        let mut x = area.x;
        for (i, col) in columns.iter().enumerate() {
            let w = col_widths[i];
            positions.push((*col, x, x + w));
            x += w + spacing;
        }
        self.column_positions.set(positions);
        
        // Header based on columns (with sort indicator)
        let header_cells: Vec<String> = columns.iter().map(|c| {
            let base = c.header();
            if self.sort_column == Some(*c) {
                format!("{} {}", base, self.sort_direction.indicator())
            } else {
                base.to_string()
            }
        }).collect();
        let header = Row::new(header_cells)
            .style(Style::new().fg(colors::ACCENT_PRIMARY).bg(colors::BG_HEADER));
        
        let table = Table::new(rows, widths)
            .header(header)
            .style(Style::new().fg(colors::FG_SECONDARY))
            .highlight_style(Style::new().fg(colors::FG_PRIMARY).bg(colors::BG_SELECTED))
            .column_spacing(1);
        
        // Render with state (need mutable access)
        let mut state = self.current_table_state().clone();
        StatefulWidget::render(&table, area, frame, &mut state);
    }
    
    fn render_detail(&self, frame: &mut Frame, area: Rect) {
        if let Some(ref item) = self.detail_item {
            let mut lines: Vec<(String, Style)> = Vec::new();
            
            // Title
            lines.push(("Title".to_string(), Style::new().fg(colors::ACCENT_PRIMARY)));
            lines.push((format!("  {}", item.title), Style::new().fg(colors::FG_PRIMARY)));
            lines.push((String::new(), Style::default()));
            
            // Status
            if let Some(completed) = item.completed {
                lines.push(("Status".to_string(), Style::new().fg(colors::ACCENT_PRIMARY)));
                let status = if completed { "  ✓ Completed" } else { "  ◯ Not completed" };
                lines.push((status.to_string(), Style::new().fg(colors::ACCENT_COMPLETED)));
                lines.push((String::new(), Style::default()));
            }
            
            // URL
            if let Some(ref url) = item.url {
                lines.push(("URL".to_string(), Style::new().fg(colors::ACCENT_PRIMARY)));
                lines.push((format!("  {}", url), Style::new().fg(colors::ACCENT_URL)));
                lines.push((String::new(), Style::default()));
            }
            
            // Tags
            if !item.tags.is_empty() {
                lines.push(("Tags".to_string(), Style::new().fg(colors::ACCENT_PRIMARY)));
                lines.push((format!("  {}", item.tags.join(", ")), Style::new().fg(colors::ACCENT_TAG)));
                lines.push((String::new(), Style::default()));
            }
            
            // Notes
            if let Some(ref notes) = item.notes {
                lines.push(("Notes".to_string(), Style::new().fg(colors::ACCENT_PRIMARY)));
                for line in notes.lines() {
                    lines.push((format!("  {}", line), Style::new().fg(colors::FG_PRIMARY)));
                }
            }
            
            // Render with scroll
            for (i, (text, style)) in lines.iter().skip(self.detail_scroll).enumerate() {
                if i >= area.height as usize {
                    break;
                }
                let y = area.y + i as u16;
                let truncated: String = text.chars().take(area.width as usize).collect();
                Paragraph::new(truncated)
                    .style(*style)
                    .render(Rect::new(area.x, y, area.width, 1), frame);
            }
        }
    }
}

fn main() {
    // Check for ASSISTANT_URL
    if env::var("ASSISTANT_URL").is_err() {
        eprintln!("Error: ASSISTANT_URL environment variable not set");
        eprintln!("Usage: ASSISTANT_URL=http://localhost:3000 cargo run --example lists_table_browser ...");
        std::process::exit(1);
    }
    
    let config = ProgramConfig {
        screen_mode: ScreenMode::AltScreen,
        mouse: true,
        ..ProgramConfig::default()
    };
    
    match Program::with_config(ListsTableBrowser::new(), config) {
        Ok(mut program) => {
            if let Err(e) = program.run() {
                eprintln!("Runtime error: {e}");
                std::process::exit(1);
            }
        }
        Err(e) => {
            eprintln!("Failed to initialize: {e}");
            std::process::exit(1);
        }
    }
}
