use super::QueueItem;

use crate::rla;
use crate::rla::ci::CiPlatform;
use regex::bytes::Regex;
use std::path::PathBuf;
use std::str;
use std::sync;

static REPO: &str = "rust-lang/rust";

pub struct Worker {
    debug_post: Option<(String, u32)>,
    index_file: PathBuf,
    index: rla::Index,
    extract_config: rla::extract::Config,
    github: rla::github::Client,
    queue: sync::mpsc::Receiver<QueueItem>,
    ci: Box<dyn CiPlatform + Send>,
}

impl Worker {
    pub fn new(
        index_file: PathBuf,
        debug_post: Option<String>,
        queue: sync::mpsc::Receiver<QueueItem>,
        ci: Box<dyn CiPlatform + Send>,
    ) -> rla::Result<Worker> {
        let debug_post = match debug_post {
            None => None,
            Some(v) => {
                let parts = v.splitn(2, '#').collect::<Vec<_>>();
                if parts.len() != 2 {
                    bail!("Invalid debug-post argument: '{}'", v);
                }

                let n = parts[1].parse()?;
                Some((parts[0].to_owned(), n))
            }
        };

        Ok(Worker {
            debug_post,
            index: rla::Index::load(&index_file)?,
            index_file,
            extract_config: Default::default(),
            github: rla::github::Client::new()?,
            queue,
            ci,
        })
    }

    pub fn main(&mut self) -> rla::Result<()> {
        loop {
            let item = self.queue.recv()?;
            match self.process(item) {
                Ok(()) => (),
                Err(e) => error!("Processing queue item failed: {}", e),
            }
        }
    }

    fn process(&mut self, item: QueueItem) -> rla::Result<()> {
        let build_id = match item {
            QueueItem::GitHubStatus(ev) => match self.ci.build_id_from_github_status(&ev) {
                Some(id) if ev.repository.full_name == REPO => id,
                _ => {
                    info!(
                        "Ignoring invalid event (ctx: {:?}, url: {:?}).",
                        ev.context, ev.target_url
                    );
                    return Ok(());
                }
            },
            QueueItem::GitHubCheckRun(ev) => match self.ci.build_id_from_github_check(&ev) {
                Some(id) if ev.repository.full_name == REPO => id,
                _ => {
                    info!(
                        "Ignoring invalid event (app id: {:?}, url: {:?}).",
                        ev.check_run.app.id, ev.check_run.details_url
                    );
                    return Ok(());
                }
            },
        };

        info!("Processing build #{}...", build_id);

        let build = self.ci.query_build(build_id)?;
        if !build.outcome().is_finished() {
            info!("Ignoring in-progress build.");
            return Ok(());
        }
        if !build.outcome().is_passed() {
            self.report_failed(build.as_ref())?;
        }
        if build.pr_number().is_none() && build.branch_name() == "auto" {
            self.learn(build.as_ref())?;
        }

        Ok(())
    }

    fn report_failed(&mut self, build: &rla::ci::Build) -> rla::Result<()> {
        debug!("Preparing report...");

        let job = match build.jobs().iter().find(|j| j.outcome().is_failed()) {
            Some(job) => *job,
            None => bail!("No failed job found, cannot report."),
        };

        let log = self.ci.query_log(job)?;

        let lines = rla::sanitize::split_lines(&log)
            .iter()
            .map(|l| rla::index::Sanitized(rla::sanitize::clean(l)))
            .collect::<Vec<_>>();

        let blocks = rla::extract::extract(&self.extract_config, &self.index, &lines);

        let blocks = blocks
            .iter()
            .map(|block| {
                block
                    .iter()
                    .map(|line| String::from_utf8_lossy(&line.0).into_owned())
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .collect::<Vec<_>>();

        let extracted = blocks.join("\n---\n");

        let commit_info = self
            .github
            .query_commit("rust-lang/rust", &build.commit_sha())?;
        let commit_message = commit_info.commit.message;

        let (pr, is_bors) = if let Some(pr) = build.pr_number() {
            (pr, false)
        } else {
            static BORS_MERGE_PREFIX: &str = "Auto merge of #";

            if commit_message.starts_with(BORS_MERGE_PREFIX) {
                let s = &commit_message[BORS_MERGE_PREFIX.len()..];
                (
                    s[..s.find(' ').ok_or_else(|| {
                        format_err!("Invalid bors commit message: '{}'", commit_message)
                    })?]
                        .parse()?,
                    true,
                )
            } else {
                bail!("Could not determine PR number, cannot report.");
            }
        };

        if !is_bors {
            let pr_info = self.github.query_pr("rust-lang/rust", pr)?;

            if !commit_message.starts_with("Merge ") {
                bail!(
                    "Did not recognize commit {} with message '{}', skipping report.",
                    build.commit_sha(),
                    commit_message
                );
            }

            let sha = commit_message
                .split(' ')
                .nth(1)
                .ok_or_else(|| {
                    format_err!(
                        "Did not recognize commit {} with message '{}', skipping report.",
                        build.commit_sha(),
                        commit_message
                    )
                })?;

            debug!("Extracted head commit sha: '{}'", sha);

            if pr_info.head.sha != sha {
                info!("Build results outdated, skipping report.");
                return Ok(());
            }
        }

        let (repo, pr) = match self.debug_post {
            Some((ref repo, pr_override)) => {
                warn!(
                    "Would post to 'rust-lang/rust#{}', debug override to '{}#{}'",
                    pr, repo, pr_override
                );
                (repo.as_ref(), pr_override)
            }
            None => ("rust-lang/rust", pr),
        };

        let opening = match extract_job_name(&lines) {
            Some(job_name) => format!("The job `{}` of your PR", job_name),
            None => "Your PR".to_owned(),
        };

        self.github.post_comment(repo, pr, &format!(r#"
{opening} [failed]({html_url}) ([raw log]({log_url})). Through arcane magic we have determined that the following fragments from the build log may contain information about the problem.

<details><summary><i>Click to expand the log.</i></summary>

```plain
{log}
```

</details><p></p>

[I'm a bot](https://github.com/rust-ops/rust-log-analyzer)! I can only do what humans tell me to, so if this was not helpful or you have suggestions for improvements, please ping or otherwise contact **`@TimNN`**. ([Feature Requests](https://github.com/rust-ops/rust-log-analyzer/issues?q=is%3Aopen+is%3Aissue+label%3Afeature-request))
        "#, opening = opening, html_url = job.html_url(), log_url = job.log_url(), log = extracted))?;

        Ok(())
    }

    fn learn(&mut self, build: &rla::ci::Build) -> rla::Result<()> {
        for job in &build.jobs() {
            if !job.outcome().is_passed() {
                continue;
            }

            debug!("Processing {}...", job);

            match self.ci.query_log(*job) {
                Ok(log) => {
                    for line in rla::sanitize::split_lines(&log) {
                        self.index
                            .learn(&rla::index::Sanitized(rla::sanitize::clean(line)), 1);
                    }
                }
                Err(e) => {
                    warn!(
                        "Failed to learn from successful {}, download failed: {}",
                        job, e
                    );
                }
            }
        }

        self.index.save(&self.index_file)?;

        Ok(())
    }
}

fn extract_job_name<I: rla::index::IndexData>(lines: &[I]) -> Option<&str> {
    lazy_static! {
        static ref JOB_NAME_PATTERN: Regex = Regex::new("\\[CI_JOB_NAME=([^\\]]+)\\]").unwrap();
    }

    for line in lines {
        if let Some(m) = JOB_NAME_PATTERN.captures(line.sanitized()) {
            return str::from_utf8(m.get(1).unwrap().as_bytes()).ok();
        }
    }

    None
}
