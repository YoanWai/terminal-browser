use std::io;

use super::Terminal;
use crate::canvas::Canvas;
use crate::kitty::Placement;
use crate::surfaces::Rect;

impl Terminal {
    pub(in crate::terminal) fn draw_full(&mut self, canvas: &Canvas, pixels: &[u8], prelude: &[u8], out: &mut Vec<u8>) -> io::Result<usize> {
        let shrank = self
            .last_frame_size
            .is_some_and(|(w, h)| canvas.width < w || canvas.height < h);
        self.last_frame_size = Some((canvas.width, canvas.height));
        // Through tmux the terminal can paint an inline frame before all of it arrives, so it goes to the hidden image first.
        let upload_id = self.back_image_id.filter(|_| self.frames_are_inline());

        let start = out.len();
        out.extend_from_slice(prelude);
        if shrank && upload_id.is_none() {
            out.extend_from_slice(&crate::kitty::kitty_delete(self.image_id, self.wrapper));
            self.clear_screen(out);
        }
        let placement = if self.wrapper.relayed() {
            let (cols, rows) = self.grid_for(canvas);
            Placement::Cells { cols, rows }
        } else {
            if self.identity.deletes_before_replace() {
                out.extend_from_slice(&crate::kitty::kitty_delete_placement(self.image_id));
            }
            out.extend_from_slice(b"\x1b[H");
            Placement::Cursor { z: 0, offset: (0, 0) }
        };
        let mut transmit = self.transmit(canvas, placement);
        if let Some(id) = upload_id {
            transmit.image_id = id;
        }
        match crate::profiler::span("kitty.handoff", || self.hand_off_frame(pixels))? {
            Some((medium, name)) => {
                out.extend_from_slice(&crate::kitty::kitty_transmit_named(transmit, &name, medium, self.wrapper));
            }
            None => out.extend_from_slice(&crate::kitty::kitty_transmit_placed(transmit, pixels, self.wrapper)),
        }
        if let Some(id) = upload_id {
            self.back_image_id = Some(self.image_id);
            self.image_id = id;
            self.placeholders = None;
            if shrank {
                self.clear_screen(out);
            }
        }
        if let Placement::Cells { cols, rows } = placement
            && self.placeholders != Some((cols, rows))
        {
            out.extend_from_slice(&crate::kitty::placeholder_grid(self.image_id, cols, rows));
            self.placeholders = Some((cols, rows));
        }
        if self.highlight_transmits {
            self.flash(out, Rect::sized(canvas.width, canvas.height), true);
        }
        crate::profiler::count("present.pixels", || canvas.width as u64 * canvas.height as u64);
        Ok(out.len() - start)
    }

    fn clear_screen(&mut self, out: &mut Vec<u8>) {
        out.extend_from_slice(b"\x1b[2J");
        if let Ok(ws) = self.size() {
            let blank_row = " ".repeat(ws.cols as usize);
            for row in 1..=ws.rows {
                out.extend_from_slice(format!("\x1b[{row};1H{blank_row}").as_bytes());
            }
        }
        self.placeholders = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::{FrameTransport, TtyHandle};
    use crate::wrapper::Wrapper;

    fn terminal(wrapper: Wrapper) -> Terminal {
        let file = std::fs::OpenOptions::new().read(true).write(true).open("/dev/null").unwrap();
        let mut terminal = Terminal::blank(TtyHandle::File(file), None, wrapper, None);
        terminal.cell = Some((8, 16));
        terminal
    }

    fn noisy_canvas(width: u32, height: u32, seed: &mut u32) -> Canvas {
        let mut canvas = Canvas::new(width, height);
        for byte in &mut canvas.pixels {
            *seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            *byte = (*seed >> 24) as u8;
        }
        canvas
    }

    #[test]
    fn tmux_uploads_inline_frames_to_the_hidden_id_before_switching_cells() {
        let mut terminal = terminal(Wrapper::Tmux);
        let ids = [terminal.back_image_id.unwrap(), terminal.image_id];
        let mut seed = 1;
        for (index, (width, height)) in [(96, 96), (96, 96), (48, 48), (96, 96)].into_iter().enumerate() {
            let canvas = noisy_canvas(width, height, &mut seed);
            let mut out = Vec::new();
            terminal.draw_full(&canvas, &canvas.pixels, &[], &mut out).unwrap();
            let text = String::from_utf8(out).unwrap();
            let (uploaded, previous) = (ids[index % 2], ids[(index + 1) % 2]);
            assert!(text.matches("m=1;").count() > 1, "the frame is sent in several chunks");
            assert!(text.contains(&format!("i={uploaded},U=1,")));
            assert!(!text.contains(&format!("i={previous},")), "the image on screen is never written to");
            assert!(!text.contains("a=d"), "nothing is deleted while it is on screen");
            let upload_done = text.find("m=0;").unwrap();
            assert!(upload_done < text.find("\x1b[38;2;").unwrap(), "cells switch after the upload");
            if index == 2 {
                assert!(upload_done < text.find("\x1b[2J").unwrap(), "a shrink clears after the upload");
            }
            let grid = crate::kitty::placeholder_grid(uploaded, width / 8, height / 16);
            assert!(text.as_bytes().ends_with(&grid));
            assert_eq!((terminal.image_id, terminal.back_image_id), (uploaded, Some(previous)));
        }
    }

    #[test]
    fn frames_sent_in_one_command_keep_one_image_id() {
        for (wrapper, transport) in [(Wrapper::None, FrameTransport::Inline), (Wrapper::Tmux, FrameTransport::File)] {
            let mut terminal = terminal(wrapper);
            terminal.transport = transport;
            let id = terminal.image_id;
            let mut seed = 1;
            for _ in 0..3 {
                let canvas = noisy_canvas(64, 64, &mut seed);
                let mut out = Vec::new();
                terminal.draw_full(&canvas, &canvas.pixels, &[], &mut out).unwrap();
                let text = String::from_utf8(out).unwrap();
                assert!(text.contains(&format!("i={id},")), "{wrapper:?} {transport:?}");
                assert_eq!(terminal.image_id, id, "{wrapper:?} {transport:?}");
            }
        }
    }
}
