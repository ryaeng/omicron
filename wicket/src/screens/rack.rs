// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The Rack presentation [`Screen`]

use super::common::CommonScreenState;
use super::Screen;
use super::ScreenId;
use super::{Height, Width};
use crate::defaults::colors::*;
use crate::defaults::style;
use crate::widgets::Control;
use crate::widgets::ControlId;
use crate::widgets::HelpMenuState;
use crate::widgets::{Banner, HelpButtonState, Rack};
use crate::wizard::{Action, Frame, ScreenEvent, State, Term};
use crate::{BOTTOM_MARGIN, TOP_MARGIN};
use crossterm::event::Event as TermEvent;
use crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use slog::Logger;
use tui::layout::Alignment;
use tui::style::{Color, Style};
use tui::widgets::{Block, Borders};

/// Show the rack view
pub struct RackScreen {
    #[allow(unused)]
    log: Logger,
    watermark: &'static str,
    hovered: Option<ControlId>,
    common: CommonScreenState,
}

impl RackScreen {
    pub fn new(log: &Logger) -> RackScreen {
        let help_data = vec![
            ("<TAB>", "Cycle forward through components"),
            ("<SHIFT>-<TAB>", "Cycle backwards through components"),
            ("<ARROWS>", "Cycle through components directionally"),
            ("<Enter> | left mouse click", "Select hovered object"),
            ("<ESC>", "Exit help menu | Reset the TabIndex of the Rack"),
            ("<CTRL-h>", "Toggle this help menu"),
            ("<CTRL-n>", "Goto the next screen"),
            ("<CTRL-c>", "Exit the program"),
        ];

        RackScreen {
            log: log.clone(),
            watermark: include_str!("../../banners/oxide.txt"),
            hovered: None,
            common: CommonScreenState {
                hovered: None,
                help_button_state: HelpButtonState::new(1, 0),
                help_menu_state: HelpMenuState::new(help_data),
                prev_screen: None,
                next_screen: ScreenId::Component,
            },
        }
    }

    fn draw_background(&self, f: &mut Frame) {
        let style = Style::default().fg(OX_GREEN_DARK).bg(OX_GRAY);
        let block = Block::default().style(style).borders(Borders::NONE);
        f.render_widget(block, f.size());
    }

    fn draw_menubar(&self, f: &mut Frame) {
        self.common.draw_menubar(f);

        // Draw the title
        let mut rect = f.size();
        let title = "Oxide Rack";
        rect.height = 1;
        rect.y = 1;
        let title_block = Block::default()
            .style(style::menu_bar())
            .title(title)
            .title_alignment(Alignment::Center);
        f.render_widget(title_block, rect);
    }

    fn draw_watermark(&self, state: &State, f: &mut Frame) -> (Height, Width) {
        let style = Style::default().fg(OX_GRAY_DARK).bg(OX_GRAY);
        let banner = Banner::new(self.watermark).style(style);
        let height = banner.height();
        let width = banner.width();
        let mut rect = f.size();

        // Only draw the banner if there is enough horizontal whitespace to
        // make it look good.
        if state.rack_state.rect().width * 3 + width > rect.width {
            return (Height(0), Width(0));
        }

        rect.x = rect.width - width - 1;
        rect.y = rect.height - height - 1;
        rect.width = width;
        rect.height = height;

        f.render_widget(banner, rect);

        (Height(height), Width(width))
    }

    /// Draw the rack in the center of the screen.
    /// Scale it to look nice.
    fn draw_rack(&self, state: &State, f: &mut Frame) {
        let rack = Rack {
            state: &state.rack_state,
            switch_style: Style::default().bg(OX_GRAY_DARK).fg(OX_WHITE),
            power_shelf_style: Style::default().bg(OX_GRAY).fg(OX_OFF_WHITE),
            sled_style: Style::default().bg(OX_GREEN_LIGHT).fg(Color::Black),
            sled_selected_style: Style::default()
                .fg(Color::Black)
                .bg(OX_GRAY_DARK),

            border_style: Style::default().fg(OX_GRAY).bg(Color::Black),
            border_selected_style: Style::default()
                .fg(OX_YELLOW)
                .bg(OX_GRAY_DARK),

            border_hover_style: Style::default().fg(OX_PINK).bg(OX_GRAY_DARK),
            switch_selected_style: Style::default().bg(OX_GRAY_DARK),
            power_shelf_selected_style: Style::default().bg(OX_GRAY),
        };

        let area = state.rack_state.rect();
        f.render_widget(rack, area);
    }

    fn handle_key_event(
        &mut self,
        state: &mut State,
        event: KeyEvent,
    ) -> Vec<Action> {
        match event.code {
            KeyCode::Tab => {
                state.rack_state.inc_tab_index();
            }
            KeyCode::BackTab => {
                state.rack_state.dec_tab_index();
            }
            KeyCode::Up => {
                state.rack_state.up_arrow();
            }
            KeyCode::Down => {
                state.rack_state.down_arrow();
            }
            KeyCode::Left | KeyCode::Right => {
                state.rack_state.left_or_right_arrow();
            }
            KeyCode::Esc => {
                if self.common.help_menu_state.is_closed() {
                    state.rack_state.clear_tab_index();
                } else {
                    self.common.help_menu_state.close();
                }
            }
            KeyCode::Enter => {
                if state.rack_state.tab_index.is_set() {
                    return vec![Action::SwitchScreen(ScreenId::Component)];
                }
            }
            KeyCode::Char('k') => {
                if event.modifiers.contains(KeyModifiers::CONTROL) {
                    state.rack_state.toggle_knight_rider_mode();
                }
            }
            _ => {
                return self.common.handle_key_event(event);
            }
        }
        vec![Action::Redraw]
    }

    fn handle_mouse_event(
        &mut self,
        state: &mut State,
        event: MouseEvent,
    ) -> Vec<Action> {
        match event.kind {
            MouseEventKind::Moved => {
                self.set_hover_state(state, event.column, event.row)
            }
            MouseEventKind::Down(MouseButton::Left) => {
                self.handle_mouse_click(state)
            }
            _ => vec![],
        }
    }

    fn handle_mouse_click(&mut self, state: &mut State) -> Vec<Action> {
        // Set the tab index to the hovered component Id if there is one.
        // Remove the old tab_index, and make it match the clicked one
        match self.hovered {
            Some(control_id) if control_id == state.rack_state.id() => {
                state.rack_state.set_tab_from_hovered();
                vec![Action::SwitchScreen(ScreenId::Component)]
            }
            _ => self.common.handle_mouse_click(),
        }
    }

    // Discover which rect the mouse is hovering over, remove any previous
    // hover state, and set any new state.
    fn set_hover_state(
        &mut self,
        state: &mut State,
        x: u16,
        y: u16,
    ) -> Vec<Action> {
        let current_id = self.find_intersection(state, x, y);
        self.common.hovered = current_id;
        if current_id == self.hovered
            && self.hovered != Some(state.rack_state.id())
        {
            // No change
            vec![]
        } else {
            self.hovered = current_id;
            if self.hovered == Some(state.rack_state.id()) {
                // Update the specific component being hovered over
                if !state.rack_state.set_hover_state(x, y) {
                    // No need to redraw, as the component is the same as before
                    vec![]
                } else {
                    vec![Action::Redraw]
                }
            } else {
                state.rack_state.hovered = None;
                vec![Action::Redraw]
            }
        }
    }

    // Return if the coordinates interesct a given control.
    // This assumes disjoint control rectangles.
    fn find_intersection(
        &self,
        state: &State,
        x: u16,
        y: u16,
    ) -> Option<ControlId> {
        if self.common.help_button_state.intersects_point(x, y) {
            Some(self.common.help_button_state.id())
        } else if state.rack_state.intersects_point(x, y) {
            Some(state.rack_state.id())
        } else {
            None
        }
    }
}

impl Screen for RackScreen {
    fn draw(&self, state: &State, terminal: &mut Term) -> anyhow::Result<()> {
        terminal.draw(|f| {
            self.draw_background(f);
            self.draw_rack(state, f);
            self.draw_watermark(state, f);
            self.draw_menubar(f);
            self.common.draw_help_menu(f);
            self.common.draw_screen_navigation_instructions(f);
            state.status_bar.draw(f);
        })?;
        Ok(())
    }

    fn on(&mut self, state: &mut State, event: ScreenEvent) -> Vec<Action> {
        match event {
            ScreenEvent::Term(TermEvent::Key(key_event)) => {
                self.handle_key_event(state, key_event)
            }
            ScreenEvent::Term(TermEvent::Mouse(mouse_event)) => {
                self.handle_mouse_event(state, mouse_event)
            }
            ScreenEvent::Tick => {
                let mut redraw = self.common.tick();

                if let Some(k) = state.rack_state.knight_rider_mode.as_mut() {
                    k.step();
                    redraw = true;
                }

                redraw |= state.status_bar.should_redraw();

                if redraw {
                    vec![Action::Redraw]
                } else {
                    vec![]
                }
            }
            ScreenEvent::Term(TermEvent::Resize(width, height)) => {
                self.common.resize(width, height);
                state.rack_state.resize(
                    width,
                    height,
                    TOP_MARGIN,
                    BOTTOM_MARGIN,
                );
                vec![Action::Redraw]
            }
            _ => vec![],
        }
    }
}
