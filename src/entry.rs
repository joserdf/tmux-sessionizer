//! Main-loop entry point for the `showrunner` binary.

use std::io;
use std::time::Duration;

use anyhow::Result;
use crossterm::ExecutableCommand;
use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEventKind, KeyModifiers,
};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use crate::app::{App, InputMode};
use crate::{cli, tmux, ui};

fn is_text_input_mode(mode: InputMode) -> bool {
    matches!(
        mode,
        InputMode::AddProjectName
            | InputMode::AddSessionName
            | InputMode::AddSessionPrompt
            | InputMode::AddAdhocSessionName
            | InputMode::AddTaskName
            | InputMode::AddTaskBranch
            | InputMode::AddTaskPrompt
            | InputMode::MergeCommitMessage
            | InputMode::SetBaseBranch
            | InputMode::Search
            | InputMode::RunCommand
    )
}

pub fn run() -> Result<()> {
    crate::config::migrate_legacy_base_dir();

    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(result) = cli::dispatch(&args) {
        return result;
    }

    let mut app = App::new()?;

    loop {
        enable_raw_mode()?;
        io::stdout().execute(EnterAlternateScreen)?;
        io::stdout().execute(EnableBracketedPaste)?;
        let backend = CrosstermBackend::new(io::stdout());
        let mut terminal = Terminal::new(backend)?;

        run_tui(&mut terminal, &mut app)?;

        io::stdout().execute(DisableBracketedPaste)?;
        disable_raw_mode()?;
        io::stdout().execute(LeaveAlternateScreen)?;

        if app.should_quit {
            break;
        }

        if let Some(session_name) = app.should_attach.take() {
            tmux::attach_session(&session_name)?;
        } else if let Some((session_name, window_idx)) = app.should_attach_window.take() {
            tmux::attach_session_window(&session_name, window_idx)?;
        } else if let Some((cwd, args, candidates)) = app.should_review_hunk.take() {
            // hunk is a terminal TUI: run it on the real terminal (the TUI is
            // suspended here), polling its live session so comments can be
            // routed to a session on exit.
            app.run_hunk_review(cwd, args, candidates);
        }
    }

    Ok(())
}

fn run_tui(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, app: &mut App) -> Result<()> {
    app.sync_worker_hints();

    loop {
        terminal.draw(|f| ui::draw(f, app))?;

        if event::poll(Duration::from_millis(100))? {
            let evt = event::read()?;

            if let Event::Paste(data) = evt {
                if is_text_input_mode(app.input_mode) {
                    let allow_newlines = matches!(
                        app.input_mode,
                        InputMode::AddTaskPrompt
                            | InputMode::AddSessionPrompt
                            | InputMode::MergeCommitMessage
                    );
                    for c in data.chars() {
                        match c {
                            '\r' => {}
                            '\n' if !allow_newlines => app.input_buffer.push(' '),
                            c if c.is_control() && c != '\n' => {}
                            c => app.input_buffer.push(c),
                        }
                    }
                }
                continue;
            }

            if let Event::Key(key) = evt {
                if key.kind != KeyEventKind::Press {
                    continue;
                }

                let kb = app.keybindings.clone();
                match app.input_mode {
                    InputMode::Normal => match key.code {
                        KeyCode::Char(c) if c == kb.quit => {
                            app.should_quit = true;
                            return Ok(());
                        }
                        KeyCode::Up => app.move_up(),
                        KeyCode::Down => app.move_down(),
                        KeyCode::Char(c) if c == kb.move_up => app.move_up(),
                        KeyCode::Char(c) if c == kb.move_down => app.move_down(),
                        KeyCode::Enter => {
                            app.enter_selected();
                            if app.should_attach.is_some() {
                                return Ok(());
                            }
                        }
                        KeyCode::Char(c) if c == kb.toggle_collapse => app.toggle_collapse(),
                        KeyCode::Char(c) if c == kb.context_menu => app.open_context_menu(),
                        KeyCode::Char(c) if c == kb.add_project => app.start_add_project(),
                        KeyCode::Char(c) if c == kb.toggle_archive_view => {
                            app.toggle_archive_view()
                        }
                        KeyCode::Char(c) if c == kb.search => app.start_search(),
                        KeyCode::Char(c) if c == kb.cycle_theme => app.cycle_theme(),
                        _ => {}
                    },
                    InputMode::ContextMenu => match key.code {
                        KeyCode::Esc => {
                            app.input_mode = InputMode::Normal;
                        }
                        KeyCode::Char(c) if c == kb.context_menu => {
                            app.input_mode = InputMode::Normal;
                        }
                        KeyCode::Up => {
                            if app.context_menu_selected > 0 {
                                app.context_menu_selected -= 1;
                            }
                        }
                        KeyCode::Char(c) if c == kb.move_up => {
                            if app.context_menu_selected > 0 {
                                app.context_menu_selected -= 1;
                            }
                        }
                        KeyCode::Down => {
                            if app.context_menu_selected + 1 < app.context_menu_items.len() {
                                app.context_menu_selected += 1;
                            }
                        }
                        KeyCode::Char(c) if c == kb.move_down => {
                            if app.context_menu_selected + 1 < app.context_menu_items.len() {
                                app.context_menu_selected += 1;
                            }
                        }
                        KeyCode::Enter => {
                            if let Some(item) =
                                app.context_menu_items.get(app.context_menu_selected)
                            {
                                let action = item.action;
                                app.execute_context_action(action);
                            }
                        }
                        KeyCode::Char(c) => {
                            if let Some(item) = app.context_menu_items.iter().find(|i| i.key == c) {
                                let action = item.action;
                                app.execute_context_action(action);
                            }
                        }
                        _ => {}
                    },
                    InputMode::AddProjectPath => match key.code {
                        KeyCode::Enter => app.confirm_add_project_path(),
                        KeyCode::Tab => app.complete_project_path(),
                        KeyCode::Esc => app.cancel_input(),
                        KeyCode::Backspace => {
                            app.input_buffer.pop();
                        }
                        KeyCode::Char(c) => app.input_buffer.push(c),
                        _ => {}
                    },
                    InputMode::AddProjectName => match key.code {
                        KeyCode::Enter => app.confirm_add_project(),
                        KeyCode::Esc => app.cancel_input(),
                        KeyCode::Backspace => {
                            app.input_buffer.pop();
                        }
                        KeyCode::Char(c) => app.input_buffer.push(c),
                        _ => {}
                    },
                    InputMode::AddTaskName => match key.code {
                        KeyCode::Enter => app.confirm_add_task(),
                        KeyCode::Esc => app.cancel_input(),
                        KeyCode::Backspace => {
                            app.input_buffer.pop();
                        }
                        KeyCode::Char(c) => app.input_buffer.push(c),
                        _ => {}
                    },
                    InputMode::AddTaskBranch => match key.code {
                        KeyCode::Enter => app.confirm_add_task_branch(),
                        KeyCode::Esc => app.cancel_input(),
                        KeyCode::Backspace => {
                            app.input_buffer.pop();
                        }
                        KeyCode::Char(c) => app.input_buffer.push(c),
                        _ => {}
                    },
                    InputMode::AddTaskPrompt => match key.code {
                        KeyCode::Enter if key.modifiers.contains(KeyModifiers::ALT) => {
                            app.input_buffer.push('\n');
                        }
                        KeyCode::Enter => app.confirm_add_task_with_prompt(),
                        KeyCode::Esc => app.cancel_input(),
                        KeyCode::Backspace => {
                            app.input_buffer.pop();
                        }
                        KeyCode::Char(c) => app.input_buffer.push(c),
                        _ => {}
                    },
                    InputMode::AddSessionName => match key.code {
                        KeyCode::Enter => {
                            app.confirm_new_session();
                        }
                        KeyCode::Esc => app.cancel_input(),
                        KeyCode::Backspace => {
                            app.input_buffer.pop();
                        }
                        KeyCode::Char(c) => app.input_buffer.push(c),
                        _ => {}
                    },
                    InputMode::AddAdhocSessionName => match key.code {
                        KeyCode::Enter => app.confirm_new_adhoc_session(),
                        KeyCode::Esc => app.cancel_input(),
                        KeyCode::Backspace => {
                            app.input_buffer.pop();
                        }
                        KeyCode::Char(c) => app.input_buffer.push(c),
                        _ => {}
                    },
                    InputMode::AddSessionPrompt => match key.code {
                        KeyCode::Enter if key.modifiers.contains(KeyModifiers::ALT) => {
                            app.input_buffer.push('\n');
                        }
                        KeyCode::Enter => {
                            app.confirm_new_session_with_prompt();
                        }
                        KeyCode::Esc => app.cancel_input(),
                        KeyCode::Backspace => {
                            app.input_buffer.pop();
                        }
                        KeyCode::Char(c) => app.input_buffer.push(c),
                        _ => {}
                    },
                    InputMode::ConfirmDelete => match key.code {
                        KeyCode::Char('y') => app.confirm_delete(),
                        KeyCode::Char('n') | KeyCode::Esc => app.cancel_input(),
                        _ => {}
                    },
                    InputMode::ConfirmCreatePr => match key.code {
                        KeyCode::Char('y') => app.confirm_create_pr(),
                        KeyCode::Char('n') | KeyCode::Esc => app.cancel_input(),
                        _ => {}
                    },
                    InputMode::MergeCommitMessage => match key.code {
                        KeyCode::Enter if key.modifiers.contains(KeyModifiers::ALT) => {
                            app.input_buffer.push('\n');
                        }
                        KeyCode::Enter => app.confirm_merge_commit(),
                        KeyCode::Esc => app.cancel_input(),
                        KeyCode::Backspace => {
                            app.input_buffer.pop();
                        }
                        KeyCode::Char(c) => app.input_buffer.push(c),
                        _ => {}
                    },
                    InputMode::SetBaseBranch => match key.code {
                        KeyCode::Enter => app.confirm_set_base_branch(),
                        KeyCode::Esc => app.cancel_input(),
                        KeyCode::Backspace => {
                            app.input_buffer.pop();
                        }
                        KeyCode::Char(c) => app.input_buffer.push(c),
                        _ => {}
                    },
                    InputMode::Search => match key.code {
                        KeyCode::Enter => app.confirm_search(),
                        KeyCode::Esc => app.cancel_search(),
                        KeyCode::Backspace => {
                            app.input_buffer.pop();
                            app.update_search();
                        }
                        KeyCode::Char(c) => {
                            app.input_buffer.push(c);
                            app.update_search();
                        }
                        _ => {}
                    },
                    InputMode::CheckoutBranch => match key.code {
                        KeyCode::Enter => app.confirm_checkout_branch(),
                        KeyCode::Esc => app.cancel_input(),
                        KeyCode::Up => app.branch_picker_move_up(),
                        KeyCode::Down => app.branch_picker_move_down(),
                        KeyCode::Backspace => {
                            app.input_buffer.pop();
                            app.update_branch_filter();
                        }
                        KeyCode::Char(c) => {
                            app.input_buffer.push(c);
                            app.update_branch_filter();
                        }
                        _ => {}
                    },
                    InputMode::RunCommand => match key.code {
                        KeyCode::Enter => {
                            app.confirm_run_command();
                            if app.should_attach.is_some() {
                                return Ok(());
                            }
                        }
                        KeyCode::Esc => app.cancel_input(),
                        KeyCode::Backspace => {
                            app.input_buffer.pop();
                        }
                        KeyCode::Char(c) => app.input_buffer.push(c),
                        _ => {}
                    },
                    InputMode::ReviewSessionPicker => match key.code {
                        KeyCode::Esc => app.cancel_review_picker(),
                        KeyCode::Up => app.review_picker_move_up(),
                        KeyCode::Down => app.review_picker_move_down(),
                        KeyCode::Char(c) if c == kb.move_up => app.review_picker_move_up(),
                        KeyCode::Char(c) if c == kb.move_down => app.review_picker_move_down(),
                        KeyCode::Enter => app.confirm_review_session(),
                        _ => {}
                    },
                    // Arrows-only navigation here (no j/k) so the `k` = Kill
                    // hotkey is reachable.
                    InputMode::RunMenu => match key.code {
                        KeyCode::Esc => app.input_mode = InputMode::Normal,
                        KeyCode::Up => {
                            if app.context_menu_selected > 0 {
                                app.context_menu_selected -= 1;
                            }
                        }
                        KeyCode::Down => {
                            if app.context_menu_selected + 1 < app.context_menu_items.len() {
                                app.context_menu_selected += 1;
                            }
                        }
                        KeyCode::Enter => {
                            if let Some(item) =
                                app.context_menu_items.get(app.context_menu_selected)
                            {
                                let action = item.action;
                                app.execute_context_action(action);
                            }
                        }
                        KeyCode::Char(c) => {
                            if let Some(item) = app.context_menu_items.iter().find(|i| i.key == c) {
                                let action = item.action;
                                app.execute_context_action(action);
                            }
                        }
                        _ => {}
                    },
                    InputMode::AgentPicker => match key.code {
                        KeyCode::Esc => app.cancel_input(),
                        KeyCode::Up => {
                            if app.context_menu_selected > 0 {
                                app.context_menu_selected -= 1;
                            }
                        }
                        KeyCode::Down => {
                            if app.context_menu_selected + 1 < app.context_menu_items.len() {
                                app.context_menu_selected += 1;
                            }
                        }
                        KeyCode::Enter => {
                            if let Some(item) =
                                app.context_menu_items.get(app.context_menu_selected)
                            {
                                let action = item.action;
                                app.execute_context_action(action);
                            }
                        }
                        KeyCode::Char(c) if c == kb.move_up => {
                            if app.context_menu_selected > 0 {
                                app.context_menu_selected -= 1;
                            }
                        }
                        KeyCode::Char(c) if c == kb.move_down => {
                            if app.context_menu_selected + 1 < app.context_menu_items.len() {
                                app.context_menu_selected += 1;
                            }
                        }
                        KeyCode::Char(c) => {
                            if let Some(item) = app.context_menu_items.iter().find(|i| i.key == c) {
                                let action = item.action;
                                app.execute_context_action(action);
                            }
                        }
                        _ => {}
                    },
                }
            }
        }

        // A context-menu action may request attaching to a session/terminal;
        // suspend the TUI so the main loop can run it.
        if app.should_attach.is_some()
            || app.should_attach_window.is_some()
            || app.should_review_hunk.is_some()
        {
            return Ok(());
        }

        // Apply background updates (non-blocking)
        app.apply_worker_updates();
        app.apply_op_results();
        app.apply_review_requests();
        app.maybe_reload_config();
        app.tick = app.tick.wrapping_add(1);

        if app.should_quit {
            return Ok(());
        }
    }
}
