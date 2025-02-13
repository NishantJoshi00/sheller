use std::{io, sync::Arc};

use crossterm::{
    event::{KeyCode, KeyModifiers},
    terminal::{EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    prelude::CrosstermBackend,
    style::{Style, Stylize},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
    Frame, Terminal,
};
use tokio::runtime::Runtime;

use crate::command::{self};

pub struct App<T: command::Execute + command::New> {
    executor: T,
    context: T::Context,
    state: State,
    runtime: Arc<Runtime>,
    history: Vec<command::CommandOutput>,
}

enum State {
    Idle(String, usize, Option<Vec<String>>), // (command, cursor_loc)
    Running(command::Prepare, Vec<String>),
}

#[derive(Debug, Default)]
enum Next {
    #[default]
    Continue,
    Exit(String),
}

impl<T: command::New + command::Execute> App<T> {
    pub fn new(rt: Runtime) -> anyhow::Result<Self> {
        let (executor, context) = T::new()?;
        Ok(Self::new_with_executor(rt, executor, context))
    }

    pub fn new_with_executor(rt: Runtime, executor: T, context: T::Context) -> Self {
        Self {
            executor,
            context,
            state: State::Idle(String::new(), 0, None),
            runtime: Arc::new(rt),
            history: Vec::new(),
        }
    }

    fn render(&self, frame: &mut Frame) {
        let prompt = self.executor.prompt(&self.context);
        let area = frame.area();
        let mut text_content = self
            .history
            .iter()
            .flat_map(render_history)
            .collect::<Vec<_>>();

        match &self.state {
            State::Idle(ref cmd, cursor, comp) => {
                let (left_cmd, right_cmd) = cmd.split_at(*cursor);
                let left_cmd = Span::styled(left_cmd, Style::default().bold());
                let (cursor, right_cmd) = match right_cmd {
                    "" => {
                        let cursor =
                            Span::styled(" ", Style::default().bg(ratatui::style::Color::White));
                        let right_cmd = Span::raw("");
                        (cursor, right_cmd)
                    }
                    right_cmd => {
                        let cursor = Span::styled(
                            right_cmd.chars().next().unwrap().to_string(),
                            Style::default()
                                .bg(ratatui::style::Color::White)
                                .fg(ratatui::style::Color::Black),
                        );

                        let right_cmd =
                            Span::styled(right_cmd[1..].to_string(), Style::default().bold());
                        (cursor, right_cmd)
                    }
                };

                text_content.push(Line::from(vec![
                    Span::styled(prompt.clone(), Style::default().blue()),
                    Span::raw(" "),
                    Span::styled(left_cmd.to_string(), Style::default().bold()),
                    cursor,
                    right_cmd,
                ]));

                if let Some(comp) = comp {
                    let completions = comp
                        .iter()
                        .map(|cmp| cmd.to_string() + cmp)
                        .map(|line| {
                            Span::styled(
                                line,
                                Style::default().bg(ratatui::style::Color::Rgb(200, 200, 200)),
                            )
                        })
                        .map(Line::from)
                        .collect::<Vec<_>>();
                    text_content.extend(completions);
                }

                let text_para = Paragraph::new(text_content).wrap(Wrap { trim: true });
                frame.render_widget(text_para, area);
            }
            State::Running(ref prep, stdin) => {
                text_content.push(Line::from(vec![
                    Span::styled(prompt.clone(), Style::default().blue()),
                    Span::raw(" "),
                    Span::styled(prep.command.clone(), Style::default().bold()),
                ]));
                let stdin = stdin
                    .iter()
                    .map(Span::raw)
                    .map(Line::from)
                    .collect::<Vec<_>>();
                text_content.extend(stdin);

                let history_para = Paragraph::new(text_content).wrap(Wrap { trim: true });
                frame.render_widget(history_para, area);
            }
        }
    }

    fn input(&mut self, event: crossterm::event::Event) -> anyhow::Result<Next> {
        // if matches!(self.state, State::Running(..)) {
        //     return Ok(Next::Continue);
        // }
        if let crossterm::event::Event::Key(ke) = event {
            match (ke.code, ke.modifiers) {
                (KeyCode::Char('l'), KeyModifiers::CONTROL) => {
                    self.history.clear();
                    return Ok(Next::Continue);
                }

                (KeyCode::Char('d') | KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                    if let State::Running(..) = &self.state {
                        self.continue_execution()?;
                    } else {
                        return Ok(Next::Exit("".to_string()));
                    }
                }
                (KeyCode::Left, KeyModifiers::NONE) => self.move_cursor_left(),
                (KeyCode::Right, KeyModifiers::NONE) => self.move_cursor_right(),
                (KeyCode::Tab, KeyModifiers::NONE) => {
                    if let State::Idle(ref mut cmd, ref mut cursor, ref mut comp @ None) =
                        self.state
                    {
                        if *cursor == cmd.len() {
                            let (fixed, variable) = self.executor.completion(&self.context, cmd)?;
                            cmd.push_str(&fixed);
                            *cursor = cmd.len();
                            *comp = Some(variable);
                        }
                    }
                }
                (KeyCode::Char(c), KeyModifiers::NONE | KeyModifiers::SHIFT) => match self.state {
                    State::Idle(ref mut cmd, ref mut cursor, ref mut comp) => {
                        cmd.insert(*cursor, c);
                        *cursor += 1;

                        match comp.as_mut() {
                            None => {}
                            Some(cmp) => {
                                *cmp = cmp
                                    .iter()
                                    .filter_map(|i| {
                                        if i.starts_with(&cmd[..*cursor]) {
                                            Some(i[*cursor..].to_string())
                                        } else {
                                            None
                                        }
                                    })
                                    .collect::<Vec<_>>();
                            }
                        }
                    }
                    State::Running(ref mut _pre, ref mut stdin) => {
                        stdin.last_mut().map(|i| i.push(c)).unwrap_or_else(|| {
                            stdin.push(c.to_string());
                        });
                    }
                },
                (KeyCode::Backspace, KeyModifiers::NONE) => {
                    self.cursor_backspace();
                }
                (KeyCode::Enter, KeyModifiers::NONE) => match self.state {
                    State::Idle(..) => {
                        self.execute_command()?;
                    }
                    State::Running(ref mut _pre, ref mut stdin) => {
                        stdin.push(String::new());
                    }
                },
                _ => {}
            }
        }

        Ok(Default::default())
    }

    pub fn execute(mut self) -> anyhow::Result<()> {
        crossterm::terminal::enable_raw_mode()?;

        let mut stdout = io::stdout();
        crossterm::execute!(stdout, EnterAlternateScreen)?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;

        let response: anyhow::Result<String> = loop {
            let draw = terminal.draw(|f| self.render(f));

            if let Err(e) = draw {
                break Err(e.into());
            }

            let event = crossterm::event::read();
            let next = match event {
                Ok(event) => self.input(event),
                Err(e) => break Err(e.into()),
            };

            match next {
                Ok(Next::Continue) => continue,
                Ok(Next::Exit(msg)) => break Ok(msg),
                Err(e) => break Err(e),
            }
        };

        crossterm::terminal::disable_raw_mode()?;
        crossterm::execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
        terminal.show_cursor()?;

        println!("{}", response?);

        Ok(())
    }

    // helpers

    fn move_cursor_left(&mut self) {
        match self.state {
            State::Idle(_, 0, _) | State::Running(..) => {}
            State::Idle(_, ref mut cursor, ref mut comp) => {
                *cursor -= 1;
                *comp = None;
            }
        }
    }

    fn move_cursor_right(&mut self) {
        match self.state {
            State::Idle(ref cmd, cursor, _) if cursor == cmd.len() => {}
            State::Idle(_, ref mut cursor, _) => {
                *cursor += 1;
            }
            State::Running(..) => {}
        }
    }

    fn cursor_backspace(&mut self) {
        match self.state {
            State::Idle(ref mut _cmd, 0, _) => {}
            State::Idle(ref mut cmd, ref mut cursor, ref mut comp) => {
                cmd.remove(*cursor - 1);
                *cursor -= 1;
                *comp = None;
            }
            State::Running(ref mut _pre, ref mut stdin) => {
                stdin.last_mut().map(|i| i.pop());
                if stdin.last().map_or(true, |i| i.is_empty()) {
                    stdin.pop();
                }
            }
        }
    }

    fn continue_execution(&mut self) -> anyhow::Result<Next> {
        let (prepare, stdin) = match self.state {
            State::Running(ref prep, ref stdin) => (prep.clone(), stdin.clone()),
            State::Idle(..) => return Ok(Next::Continue),
        };

        self._final_execution(&prepare.command, Some(stdin))
    }

    fn execute_command(&mut self) -> anyhow::Result<Next> {
        let (cmd, _) = match self.state {
            State::Idle(ref cmd, cursor, _) => (cmd.clone(), cursor),
            State::Running(..) => return Ok(Next::Continue),
        };

        let prepare = self.executor.prepare(&cmd);
        self.state = State::Running(prepare.clone(), Vec::new());

        match prepare.stdin_required {
            true => Ok(Next::Continue),
            false => self._final_execution(&cmd, None),
        }
    }

    fn _final_execution(&mut self, cmd: &str, stdin: Option<Vec<String>>) -> anyhow::Result<Next> {
        let prompt = self.executor.prompt(&self.context);
        let output = self.executor.execute(
            &mut self.context,
            command::CommandInput {
                prompt,
                command: cmd.to_string(),
                stdin,
                runtime: self.runtime.clone(),
            },
        )?;
        self.state = State::Idle(String::new(), 0, None);

        match output {
            command::OutputAction::Command(command_output) => self.history.push(command_output),
            command::OutputAction::Exit => {
                return Ok(Next::Exit("".to_string()));
            }
            command::OutputAction::Clear => {
                self.history.clear();
            }
        }

        Ok(Next::Continue)
    }
}

fn render_history(history: &command::CommandOutput) -> Vec<Line> {
    let command = Line::from(vec![
        Span::styled(history.prompt.clone(), Style::default().blue()),
        Span::raw(" "),
        Span::styled(history.command.clone(), Style::default().bold()),
    ]);
    let stdin = history
        .stdin
        .iter()
        .cloned()
        .map(Span::raw)
        .map(Line::from)
        .collect::<Vec<_>>();
    let stdout = history
        .stdout
        .iter()
        .cloned()
        .map(Span::raw)
        .map(Line::from)
        .collect::<Vec<_>>();
    let stderr = history
        .stderr
        .iter()
        .cloned()
        .map(|i| Span::styled(i, Style::default().red()))
        .map(Line::from)
        .collect::<Vec<_>>();

    let mut lines = vec![command];
    lines.extend(stdin);
    lines.extend(stdout);
    lines.extend(stderr);

    lines
}
