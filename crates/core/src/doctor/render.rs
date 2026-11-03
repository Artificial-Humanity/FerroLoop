//! The human report. Sorted by the fixed check order; no timestamps.

use super::DoctorReport;

impl DoctorReport {
    /// The text `fl doctor` prints without `--json`.
    pub fn render_text(&self) -> String {
        let mut out = String::from("fl doctor (observational: nothing was changed)\n\n");
        for c in &self.checks {
            out.push_str(&format!(
                "[{}] {}  {}\n      {}\n",
                c.status.as_str(),
                c.id,
                c.title,
                c.detail
            ));
        }
        let s = &self.summary;
        out.push_str(&format!(
            "\noverall: {} (pass {}, warn {}, fail {}, not-installed {}, unsupported {})\n",
            self.overall.as_str(),
            s.pass,
            s.warn,
            s.fail,
            s.not_installed,
            s.unsupported
        ));
        out
    }
}
