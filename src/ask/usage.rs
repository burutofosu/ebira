//! The spending ledger: one line per API call, so the credit can be watched (`ebira ask-usage`).
//! Each line names the run that made it, so the cost of one question can be told apart from
//! the calls it took.

use super::api::Usage;
use crate::{json, private_fs, time};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const HEADER: &str = "time_utc\tstep\tmodel\tinput_tokens\toutput_tokens\tcache_read_tokens\tcache_write_tokens\tusd\tquestion\trun";

pub struct Ledger {
    path: PathBuf,
    /// This process: every call one `ebira ask` makes carries it.
    run: String,
}

impl Ledger {
    pub fn new(path: PathBuf) -> Ledger {
        Ledger {
            path,
            run: format!("{}-{}", now_ms(), std::process::id()),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn record(
        &self,
        step: &str,
        model: &str,
        usage: &Usage,
        usd: Option<f64>,
        question: &str,
    ) -> io::Result<()> {
        if let Some(directory) = self.path.parent() {
            private_fs::create_dir_all(directory)?;
        }
        let new = !self.path.exists();
        let mut file = private_fs::write_options().append(true).open(&self.path)?;
        let mut line = String::new();
        if new {
            line.push_str(HEADER);
            line.push('\n');
        }
        let question: String = question
            .chars()
            .map(|c| {
                if c == '\t' || c == '\n' || c == '\r' {
                    ' '
                } else {
                    c
                }
            })
            .take(80)
            .collect();
        line.push_str(&format!(
            "{}\t{step}\t{model}\t{}\t{}\t{}\t{}\t{}\t{question}\t{}\n",
            time::rfc3339_from_unix_ms(now_ms(), 0),
            usage.input,
            usage.output,
            usage.cache_read,
            usage.cache_write,
            usd.map(|value| format!("{value:.6}")).unwrap_or_default(),
            self.run,
        ));
        file.write_all(line.as_bytes())
    }

    /// What the ledger holds, summed.
    pub fn spending(&self) -> Spending {
        let text = fs::read_to_string(&self.path).unwrap_or_default();
        let week_ago = (now_ms() - 7 * 86_400_000) as i128 * 1_000_000;
        let mut spending = Spending::default();
        let mut runs = BTreeSet::new();
        for line in text.lines().skip(1) {
            let fields: Vec<&str> = line.split('\t').collect();
            if fields.len() < 9 {
                continue;
            }
            let Ok(usd) = fields[7].parse::<f64>() else {
                spending.unpriced += 1;
                continue;
            };
            spending.total += usd;
            if time::instant(fields[0], 0).is_some_and(|at| at >= week_ago) {
                spending.week += usd;
            }
            *spending.by_model.entry(fields[2].to_string()).or_default() += usd;
            *spending.by_step.entry(fields[1].to_string()).or_default() += usd;
            // Lines written before runs were recorded belong to no question.
            if let Some(run) = fields.get(9).filter(|run| !run.is_empty()) {
                spending.question_usd += usd;
                runs.insert(run.to_string());
            }
        }
        spending.questions = runs.len();
        spending
    }
}

#[derive(Default)]
pub struct Spending {
    pub total: f64,
    pub week: f64,
    /// Calls of models the price table does not know: not in the sums.
    pub unpriced: usize,
    /// Questions asked since runs were recorded, and what they cost.
    pub questions: usize,
    pub question_usd: f64,
    pub by_model: BTreeMap<String, f64>,
    pub by_step: BTreeMap<String, f64>,
}

impl Spending {
    pub fn per_question(&self) -> Option<f64> {
        (self.questions > 0).then(|| self.question_usd / self.questions as f64)
    }

    pub fn json(&self, ledger: &Path, budget: Option<f64>) -> String {
        let amounts = |values: &BTreeMap<String, f64>, key: &str| {
            json::array(values.iter().map(|(name, usd)| {
                json::Object::new()
                    .name(key, name)
                    .raw("usd", &usd_json(*usd))
                    .finish()
            }))
        };
        json::Object::new()
            .name("disposition", "ready")
            .name("mode", "ask_usage")
            .name("ledger", &ledger.to_string_lossy())
            .raw("total_usd", &usd_json(self.total))
            .raw("last_7_days_usd", &usd_json(self.week))
            .number("questions", self.questions as u64)
            .raw(
                "usd_per_question",
                &json::or_null(self.per_question().map(usd_json)),
            )
            .raw("by_model", &amounts(&self.by_model, "model"))
            .raw("by_step", &amounts(&self.by_step, "step"))
            .number("unpriced_calls", self.unpriced as u64)
            .raw("budget_usd", &json::or_null(budget.map(usd_json)))
            .raw(
                "remaining_usd",
                &json::or_null(budget.map(|budget| usd_json(budget - self.total))),
            )
            .finish()
    }

    pub fn text(&self, ledger: &Path, budget: Option<f64>) -> String {
        let mut out = format!(
            "What `ebira ask` spent (ledger: {})\nTotal: ${:.3} (last 7 days: ${:.3})\n",
            ledger.display(),
            self.total,
            self.week
        );
        if let Some(average) = self.per_question() {
            out.push_str(&format!(
                "Questions: {} (${average:.3} each, searching and checking included)\n",
                self.questions
            ));
        }
        if !self.by_model.is_empty() {
            let models: Vec<String> = self
                .by_model
                .iter()
                .map(|(model, usd)| format!("{model} ${usd:.3}"))
                .collect();
            out.push_str(&format!("By model: {}\n", models.join(" / ")));
            let steps: Vec<String> = self
                .by_step
                .iter()
                .map(|(step, usd)| format!("{step} ${usd:.3}"))
                .collect();
            out.push_str(&format!("By step: {}\n", steps.join(" / ")));
        }
        if self.unpriced > 0 {
            out.push_str(&format!(
                "Calls of models the price table does not know: {} (not in the total)\n",
                self.unpriced
            ));
        }
        if let Some(budget) = budget {
            out.push_str(&format!(
                "Left of ${budget:.0} (EBIRA_ASK_BUDGET_USD), counting this ledger only: ${:.2}\n",
                budget - self.total
            ));
        }
        out.push_str(
            "Prices are estimates from the published rates; the Console has the billed amounts.\n",
        );
        out
    }
}

fn usd_json(usd: f64) -> String {
    format!("{usd:.4}")
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_and_sums_spending_per_question() {
        let directory =
            std::env::temp_dir().join(format!("ebira-ask-ledger-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        let path = directory.join("usage.tsv");
        fs::create_dir_all(&directory).unwrap();
        // A line from before runs were recorded: in the total, in no question.
        fs::write(
            &path,
            format!("{HEADER}\n2026-10-15T00:00:00Z\tanswer\tclaude-opus-5-5\t1\t1\t0\t0\t1.000000\told\n"),
        )
        .unwrap();
        let usage = Usage {
            input: 1000,
            output: 100,
            ..Usage::default()
        };
        let first = Ledger::new(path.clone());
        first
            .record("plan", "claude-opus-5-5", &usage, Some(0.006), "地図の\t色")
            .unwrap();
        first
            .record("answer", "claude-opus-5-5", &usage, Some(0.25), "地図の色")
            .unwrap();
        first
            .record("screen", "mystery-model", &usage, None, "地図の色")
            .unwrap();
        let second = Ledger {
            path: path.clone(),
            run: "other-run".to_string(),
        };
        second
            .record("answer", "claude-haiku-5-5", &usage, Some(0.044), "q")
            .unwrap();
        let spending = first.spending();
        assert!((spending.total - 1.3).abs() < 1e-9);
        assert_eq!(spending.questions, 2);
        assert!((spending.per_question().unwrap() - 0.15).abs() < 1e-9);
        assert_eq!(spending.unpriced, 1);
        let text = spending.text(&path, Some(200.0));
        assert!(text.contains("Total: $1.300"), "{text}");
        assert!(text.contains("Questions: 2 ($0.150 each"), "{text}");
        assert!(text.contains("$198.70"), "{text}");
        let json = spending.json(&path, None);
        assert!(json.contains("\"questions\":2"), "{json}");
        assert!(json.contains("\"budget_usd\":null"), "{json}");
        let _ = fs::remove_dir_all(&directory);
    }
}
