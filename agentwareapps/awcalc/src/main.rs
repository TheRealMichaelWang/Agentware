//! A calculator: the first real application.
//!
//! It is deliberately the simplest thing that is honestly an application: a
//! display, a grid of buttons, and immediate-execution arithmetic, the way a
//! pocket calculator works. `1 + 2 + 3 =` shows 3 the moment the second `+` is
//! pressed, because each operator settles what came before it.
//!
//! The shape to notice is the same one the reference clients have: a model and
//! a `render`, nothing in between. Every press resends the whole interface and
//! the compositor diffs it. Ids are written by hand and never change, because
//! they are what an agent names to act: `digit-7`, `add`, `equals`.
//!
//! The equals button turns itself off when there is nothing pending. That is
//! not cosmetic: a disabled control offers no actions, so an agent reading this
//! window can see that `=` exists, what it would do, and that pressing it now
//! would mean nothing.
//!
//! Usage: awcalc

use std::fmt::Write as _;
use std::io::Write as IoWrite;

use awproto::display::{self, Event, Surface};

// The ink palette, invented for this app rather than borrowed from anywhere:
// warm bone for numbers, verdigris for the operators, copper for equals and a
// rust red for clear, like kiln-fired ceramic on the system's dark slate.
//
// Ink is the whole of an application's styling power, and that is by design:
// button fills, borders and the pressed animation are the compositor's, so an
// app can be expressive without being able to make a control lie about what it
// is. None of these colours reach an agent.
const INK_DISPLAY: &str = "#f0e7cd";
const INK_PENDING: &str = "#8f8672";
const INK_DIGIT: &str = "#ded3b8";
const INK_OPERATOR: &str = "#7cb9a3";
const INK_EQUALS: &str = "#d29455";
const INK_CLEAR: &str = "#c26a50";

/// Longer than this stops fitting in the window, so further digits are ignored
/// the way a pocket calculator's are when its display is full.
const DISPLAY_MAX: usize = 15;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Op {
    Add,
    Subtract,
    Multiply,
    Divide,
}

impl Op {
    fn symbol(self) -> char {
        match self {
            Op::Add => '+',
            Op::Subtract => '−',
            Op::Multiply => '×',
            Op::Divide => '÷',
        }
    }

    fn apply(self, lhs: f64, rhs: f64) -> f64 {
        match self {
            Op::Add => lhs + rhs,
            Op::Subtract => lhs - rhs,
            Op::Multiply => lhs * rhs,
            Op::Divide => lhs / rhs,
        }
    }
}

struct Calculator {
    /// What the display shows: the number being entered, or the last result,
    /// or "Error".
    display: String,
    /// The settled left-hand side, once an operator has been pressed.
    lhs: Option<f64>,
    op: Option<Op>,
    /// True when the next digit starts a new number instead of extending the
    /// one on the display, which is the state after an operator or equals.
    fresh: bool,
}

fn main() {
    let mut surface = match Surface::inherited() {
        Ok(surface) => surface,
        Err(err) => {
            log(&format!("no interface connection: {err}"));
            std::process::exit(1);
        }
    };

    let mut calc = Calculator {
        display: "0".to_owned(),
        lhs: None,
        op: None,
        fresh: true,
    };

    if let Err(err) = surface.render(&calc.render()) {
        log(&format!("could not send the first tree: {err}"));
        std::process::exit(1);
    }

    loop {
        let event = match surface.next_event() {
            Ok(Some(event)) => event,
            Ok(None) => {
                log("the compositor closed the connection");
                return;
            }
            Err(err) => {
                log(&format!("connection failed: {err}"));
                std::process::exit(1);
            }
        };

        // The cross on the window. It asks rather than closing, because an
        // application may have something to lose; this one never does, so it
        // answers by going. An application that says nothing here is closed by
        // the human's next press instead, which is a click they should not have
        // to spend.
        if event.action == display::ACTION_CLOSE && event.target.is_empty() {
            return;
        }

        if !calc.accept(&surface, &event) {
            continue;
        }

        if let Err(err) = surface.render(&calc.render()) {
            log(&format!("could not send a tree: {err}"));
            return;
        }
    }
}

impl Calculator {
    /// Act on an event, or say that nothing changed.
    ///
    /// A stale click is discarded rather than acted on: its meaning came from a
    /// tree that no longer exists. Every control here is a button, so unlike a
    /// text field there is no event whose payload outlives its tree.
    fn accept(&mut self, surface: &Surface, event: &Event) -> bool {
        if event.action != display::ACTION_CLICK {
            return false;
        }
        if surface.is_stale(event) {
            log(&format!(
                "discarded a click on {} against v{}, now on v{}",
                event.target,
                event.version,
                surface.version()
            ));
            return true;
        }

        match event.target.as_str() {
            "dot" => self.dot(),
            "clear" => self.clear(),
            "equals" => self.equals(),
            "add" => self.operator(Op::Add),
            "subtract" => self.operator(Op::Subtract),
            "multiply" => self.operator(Op::Multiply),
            "divide" => self.operator(Op::Divide),
            target => match target.strip_prefix("digit-").and_then(|d| d.chars().next()) {
                Some(digit) if digit.is_ascii_digit() => self.digit(digit),
                _ => {
                    log(&format!("nothing here answers to {target}"));
                    return false;
                }
            },
        }
        true
    }

    fn digit(&mut self, digit: char) {
        if self.fresh || self.display == "0" {
            self.display = digit.to_string();
            self.fresh = false;
        } else if self.display.chars().count() < DISPLAY_MAX {
            self.display.push(digit);
        }
        log(&format!("display {}", self.display));
    }

    fn dot(&mut self) {
        if self.fresh {
            self.display = "0.".to_owned();
            self.fresh = false;
        } else if !self.display.contains('.') {
            self.display.push('.');
        }
        log(&format!("display {}", self.display));
    }

    fn clear(&mut self) {
        self.display = "0".to_owned();
        self.lhs = None;
        self.op = None;
        self.fresh = true;
        log("cleared");
    }

    /// An operator settles whatever is pending, then waits for the next number.
    fn operator(&mut self, op: Op) {
        let Ok(current) = self.display.parse::<f64>() else {
            // The display says "Error". Nothing to build on until clear or a
            // digit replaces it.
            log("no number to operate on");
            return;
        };

        // `1 + 2 ×` computes 3 first: immediate execution, no precedence. The
        // exception is pressing two operators in a row, which is a correction
        // rather than a calculation, so the newer one simply wins.
        let lhs = match (self.lhs, self.op) {
            (Some(lhs), Some(pending)) if !self.fresh => pending.apply(lhs, current),
            (Some(lhs), Some(_)) => lhs,
            _ => current,
        };
        if !lhs.is_finite() {
            self.error();
            return;
        }

        self.display = format_number(lhs);
        self.lhs = Some(lhs);
        self.op = Some(op);
        self.fresh = true;
        log(&format!("pending {} {}", self.display, op.symbol()));
    }

    fn equals(&mut self) {
        // Unreachable through the interface: the button is disabled whenever
        // nothing is pending, so neither a click nor an intent can get here.
        let (Some(lhs), Some(op)) = (self.lhs, self.op) else {
            log("nothing pending");
            return;
        };
        let Ok(rhs) = self.display.parse::<f64>() else {
            log("no number to operate on");
            return;
        };

        let result = op.apply(lhs, rhs);
        if !result.is_finite() {
            self.error();
            return;
        }

        log(&format!(
            "{} {} {} = {}",
            format_number(lhs),
            op.symbol(),
            format_number(rhs),
            format_number(result)
        ));
        self.display = format_number(result);
        self.lhs = None;
        self.op = None;
        self.fresh = true;
    }

    /// Division by zero, or a result too large to mean anything.
    fn error(&mut self) {
        self.display = "Error".to_owned();
        self.lhs = None;
        self.op = None;
        self.fresh = true;
        log("error");
    }

    /// The whole interface, from the model, every time.
    fn render(&self) -> String {
        // The line above the display shows what is waiting for its other half:
        // "12 +" while the 34 is being typed. Empty the rest of the time, and
        // sent empty rather than omitted so the layout never jumps.
        let pending = match (self.lhs, self.op) {
            (Some(lhs), Some(op)) => format!("{} {}", format_number(lhs), op.symbol()),
            _ => String::new(),
        };

        let mut out = String::new();
        let _ = write!(
            out,
            r#"<window title="Calculator" font="sans">
  <vstack gap="sm">
    <text role="caption" color="{INK_PENDING}">{pending}</text>
    <text role="heading" font="mono" size="26" color="{INK_DISPLAY}">{display}</text>
    <divider/>
"#,
            pending = display::escape(&pending),
            display = display::escape(&self.display),
        );

        // Four rows of four, then equals across the bottom. Every button in a
        // row grows, so the columns come out even instead of each button
        // hugging its one-character label.
        let rows: [[(&str, &str, &str, &str); 4]; 4] = [
            [
                ("digit-7", "7", INK_DIGIT, "Enters the digit 7"),
                ("digit-8", "8", INK_DIGIT, "Enters the digit 8"),
                ("digit-9", "9", INK_DIGIT, "Enters the digit 9"),
                ("divide", "÷", INK_OPERATOR, "Divides the current number by the next one entered"),
            ],
            [
                ("digit-4", "4", INK_DIGIT, "Enters the digit 4"),
                ("digit-5", "5", INK_DIGIT, "Enters the digit 5"),
                ("digit-6", "6", INK_DIGIT, "Enters the digit 6"),
                ("multiply", "×", INK_OPERATOR, "Multiplies the current number by the next one entered"),
            ],
            [
                ("digit-1", "1", INK_DIGIT, "Enters the digit 1"),
                ("digit-2", "2", INK_DIGIT, "Enters the digit 2"),
                ("digit-3", "3", INK_DIGIT, "Enters the digit 3"),
                ("subtract", "−", INK_OPERATOR, "Subtracts the next number entered from the current one"),
            ],
            [
                ("clear", "C", INK_CLEAR, "Clears the display and forgets the calculation in progress"),
                ("digit-0", "0", INK_DIGIT, "Enters the digit 0"),
                ("dot", ".", INK_DIGIT, "Starts the decimal part of the number being entered"),
                ("add", "+", INK_OPERATOR, "Adds the current number and the next one entered"),
            ],
        ];

        for row in rows {
            out.push_str("    <hstack gap=\"sm\">\n");
            for (id, label, ink, description) in row {
                let _ = writeln!(
                    out,
                    r#"      <button id="{id}" label="{label}" grow="true" color="{ink}"
              description="{description}"/>"#,
                );
            }
            out.push_str("    </hstack>\n");
        }

        let _ = write!(
            out,
            r#"    <button id="equals" label="=" color="{INK_EQUALS}"{disabled}
            description="Computes the result of the pending calculation"/>
  </vstack>
</window>
"#,
            disabled = if self.op.is_some() { "" } else { " disabled" },
        );
        out
    }
}

/// A number the way a calculator shows one: no float noise, no trailing zeros.
///
/// `0.1 + 0.2` must read 0.3 whatever the doubles underneath think, so results
/// are rounded to ten decimal places before printing, and a whole number prints
/// without a fractional part at all.
fn format_number(value: f64) -> String {
    let rounded = (value * 1e10).round() / 1e10;
    if rounded == rounded.trunc() && rounded.abs() < 1e15 {
        format!("{}", rounded as i64)
    } else {
        format!("{rounded}")
    }
}

/// Log to the kernel ring buffer.
///
/// An application has no console worth writing to: the haimanager owns the
/// screen and stdout goes to a terminal that is no longer being displayed.
fn log(message: &str) {
    let line = format!("<6>awcalc: {message}\n");
    if let Ok(mut kmsg) = std::fs::OpenOptions::new().write(true).open("/dev/kmsg") {
        let _ = kmsg.write_all(line.as_bytes());
    } else {
        eprintln!("awcalc: {message}");
    }
}
