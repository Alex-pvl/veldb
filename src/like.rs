//! SQL `LIKE`: `%` — любая последовательность, `_` — один символ.

/// Компилированный шаблон. Компиляция один раз на запрос, применение — на строку.
#[derive(Debug, Clone)]
pub enum Pattern {
    /// Быстрый путь: в шаблоне нет `_`, значит достаточно искать литеральные куски.
    /// `%a%b` → куски `["a","b"]`, привязка к началу/концу по краевым `%`.
    Chunks {
        parts: Vec<String>,
        anchored_start: bool,
        anchored_end: bool,
    },
    /// Общий случай с `_`: посимвольный автомат.
    Chars(Vec<char>),
}

impl Pattern {
    pub fn compile(pat: &str) -> Pattern {
        if pat.contains('_') {
            return Pattern::Chars(pat.chars().collect());
        }
        let parts: Vec<String> = pat
            .split('%')
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect();
        Pattern::Chunks {
            parts,
            anchored_start: !pat.starts_with('%'),
            anchored_end: !pat.ends_with('%'),
        }
    }

    pub fn matches(&self, s: &str) -> bool {
        match self {
            Pattern::Chunks {
                parts,
                anchored_start,
                anchored_end,
            } => {
                if parts.is_empty() {
                    // Шаблон из одних `%` (или пустой) — совпадает со всем,
                    // кроме случая полностью пустого шаблона против непустой строки.
                    return !(*anchored_start && *anchored_end) || s.is_empty();
                }
                let mut rest = s;
                for (i, part) in parts.iter().enumerate() {
                    let last = i + 1 == parts.len();
                    if i == 0 && *anchored_start {
                        match rest.strip_prefix(part.as_str()) {
                            Some(r) => rest = r,
                            None => return false,
                        }
                    } else if last && *anchored_end {
                        // Хвост проверяем на месте: искать `find` тут нельзя, совпасть
                        // должен именно конец строки.
                        return rest.len() >= part.len() && rest.ends_with(part.as_str());
                    } else {
                        match rest.find(part.as_str()) {
                            Some(p) => rest = &rest[p + part.len()..],
                            None => return false,
                        }
                    }
                }
                // Сюда попадаем, когда последний кусок съел префиксный разбор
                // (шаблон вида `abc` — привязан с обеих сторон одним куском).
                !*anchored_end || rest.is_empty()
            }
            Pattern::Chars(p) => {
                let s: Vec<char> = s.chars().collect();
                match_chars(&s, p)
            }
        }
    }
}

/// Классический жадный матчер с одной точкой возврата на последнюю `%`.
/// Линейный в среднем и не рекурсивный — важно, чтобы шаблон из пользователя
/// не мог уронить сервер по стеку.
fn match_chars(s: &[char], p: &[char]) -> bool {
    let (mut si, mut pi) = (0usize, 0usize);
    let (mut star_pi, mut star_si) = (usize::MAX, 0usize);
    while si < s.len() {
        if pi < p.len() && (p[pi] == '_' || p[pi] == s[si]) {
            si += 1;
            pi += 1;
        } else if pi < p.len() && p[pi] == '%' {
            star_pi = pi;
            star_si = si;
            pi += 1;
        } else if star_pi != usize::MAX {
            star_si += 1;
            si = star_si;
            pi = star_pi + 1;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == '%')
}
