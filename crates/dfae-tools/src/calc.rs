//! Safe arithmetic evaluator (recursive descent; no eval, no code execution).

pub fn evaluate(src: &str) -> Result<f64, String> {
    if src.len() > 200 {
        return Err("expression too long".into());
    }
    let normalized: String = src
        .chars()
        .map(|c| match c {
            '×' | 'x' | 'X' => '*',
            '÷' => '/',
            '−' => '-',
            c => c,
        })
        .collect();
    let mut p = Parser { s: normalized.chars().collect(), i: 0, depth: 0 };
    let v = p.expr()?;
    p.ws();
    if p.i != p.s.len() {
        return Err(format!("unexpected '{}' at position {}", p.s[p.i], p.i));
    }
    if v.is_finite() { Ok(v) } else { Err("result is not a finite number".into()) }
}

pub fn format_number(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        let s = format!("{v:.10}");
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

struct Parser {
    s: Vec<char>,
    i: usize,
    depth: usize,
}

impl Parser {
    fn ws(&mut self) {
        while self.i < self.s.len() && self.s[self.i].is_whitespace() {
            self.i += 1;
        }
    }
    fn peek(&mut self) -> Option<char> {
        self.ws();
        self.s.get(self.i).copied()
    }
    fn expr(&mut self) -> Result<f64, String> {
        let mut v = self.term()?;
        while let Some(c) = self.peek() {
            match c {
                '+' => {
                    self.i += 1;
                    v += self.term()?;
                }
                '-' => {
                    self.i += 1;
                    v -= self.term()?;
                }
                _ => break,
            }
        }
        Ok(v)
    }
    fn term(&mut self) -> Result<f64, String> {
        let mut v = self.power()?;
        while let Some(c) = self.peek() {
            match c {
                '*' => {
                    self.i += 1;
                    v *= self.power()?;
                }
                '/' => {
                    self.i += 1;
                    let d = self.power()?;
                    if d == 0.0 {
                        return Err("division by zero".into());
                    }
                    v /= d;
                }
                '%' => {
                    self.i += 1;
                    let d = self.power()?;
                    if d == 0.0 {
                        return Err("modulo by zero".into());
                    }
                    v %= d;
                }
                _ => break,
            }
        }
        Ok(v)
    }
    fn power(&mut self) -> Result<f64, String> {
        let base = self.unary()?;
        if self.peek() == Some('^') {
            self.i += 1;
            let exp = self.power()?; // right associative
            return Ok(base.powf(exp));
        }
        Ok(base)
    }
    fn unary(&mut self) -> Result<f64, String> {
        match self.peek() {
            Some('-') => {
                self.i += 1;
                Ok(-self.unary()?)
            }
            Some('+') => {
                self.i += 1;
                self.unary()
            }
            _ => self.primary(),
        }
    }
    fn primary(&mut self) -> Result<f64, String> {
        self.depth += 1;
        if self.depth > 64 {
            return Err("expression nested too deeply".into());
        }
        let r = self.primary_inner();
        self.depth -= 1;
        r
    }
    fn primary_inner(&mut self) -> Result<f64, String> {
        match self.peek() {
            Some('(') => {
                self.i += 1;
                let v = self.expr()?;
                if self.peek() != Some(')') {
                    return Err("missing closing parenthesis".into());
                }
                self.i += 1;
                Ok(v)
            }
            Some(c) if c.is_ascii_digit() || c == '.' => {
                let st = self.i;
                while self.i < self.s.len() && (self.s[self.i].is_ascii_digit() || self.s[self.i] == '.') {
                    self.i += 1;
                }
                let t: String = self.s[st..self.i].iter().collect();
                t.parse::<f64>().map_err(|_| format!("bad number '{t}'"))
            }
            Some(c) if c.is_alphabetic() => {
                let st = self.i;
                while self.i < self.s.len() && self.s[self.i].is_alphanumeric() {
                    self.i += 1;
                }
                let name: String = self.s[st..self.i].iter().collect::<String>().to_lowercase();
                match name.as_str() {
                    "pi" => return Ok(std::f64::consts::PI),
                    "e" => return Ok(std::f64::consts::E),
                    _ => {}
                }
                if self.peek() != Some('(') {
                    return Err(format!("unknown identifier '{name}'"));
                }
                self.i += 1;
                let a = self.expr()?;
                if self.peek() != Some(')') {
                    return Err("missing closing parenthesis".into());
                }
                self.i += 1;
                match name.as_str() {
                    "sqrt" if a >= 0.0 => Ok(a.sqrt()),
                    "sqrt" => Err("sqrt of a negative number".into()),
                    "abs" => Ok(a.abs()),
                    "sin" => Ok(a.sin()),
                    "cos" => Ok(a.cos()),
                    "tan" => Ok(a.tan()),
                    "ln" if a > 0.0 => Ok(a.ln()),
                    "log" | "log10" if a > 0.0 => Ok(a.log10()),
                    "round" => Ok(a.round()),
                    "floor" => Ok(a.floor()),
                    "ceil" => Ok(a.ceil()),
                    _ => Err(format!("unknown function '{name}'")),
                }
            }
            Some(c) => Err(format!("unexpected '{c}'")),
            None => Err("unexpected end of expression".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evaluates() {
        assert_eq!(evaluate("2 + 3 * 4").unwrap(), 14.0);
        assert_eq!(evaluate("(2 + 3) * 4").unwrap(), 20.0);
        assert_eq!(evaluate("2 ^ 3 ^ 2").unwrap(), 512.0);
        assert_eq!(evaluate("-3 + 5").unwrap(), 2.0);
        assert_eq!(evaluate("sqrt(81) + 10 % 4").unwrap(), 11.0);
        assert_eq!(evaluate("12 x 3").unwrap(), 36.0);
        assert!((evaluate("pi * 2").unwrap() - std::f64::consts::TAU).abs() < 1e-9);
    }

    #[test]
    fn rejects_bad_input() {
        assert!(evaluate("1 / 0").is_err());
        assert!(evaluate("2 +").is_err());
        assert!(evaluate("system(1)").is_err());
        assert!(evaluate("foo").is_err());
        assert!(evaluate(&"(".repeat(100)).is_err());
    }

    #[test]
    fn formats() {
        assert_eq!(format_number(14.0), "14");
        assert_eq!(format_number(0.1 + 0.2), "0.3");
        assert_eq!(format_number(1.0 / 3.0), "0.3333333333");
    }
}
