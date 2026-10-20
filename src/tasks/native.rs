//! Native [`TaskProvider`]: a board backed by showrunner's own tasks.
//!
//! A task becomes a card only when it has an explicit `column`; human-made
//! tasks (which never set one) stay invisible to boards and the dispatcher.
//! The provider holds a `&mut Project` so all mutations land directly in the
//! in-memory config; a separate `Config::save()` persists them, which is what
//! lets tests run purely in memory.

use anyhow::{Result, anyhow};

use crate::config::Project;
use crate::tasks::{Card, CardId, Column, Link, TaskLink, TaskProvider};

/// Board provider over one showrunner project.
pub struct NativeProvider<'a> {
    pub project: &'a mut Project,
}

impl TaskProvider for NativeProvider<'_> {
    fn list(&self) -> Result<Vec<Card>> {
        Ok(self
            .project
            .tasks
            .iter()
            .filter_map(|t| {
                t.column.map(|column| Card {
                    id: CardId(t.branch.clone()),
                    key: t.name.clone(),
                    title: t.name.clone(),
                    body: t.body.clone().unwrap_or_default(),
                    column,
                    labels: vec![],
                    priority: None,
                    url: None,
                })
            })
            .collect())
    }

    fn move_to(&mut self, card: &CardId, col: Column) -> Result<()> {
        let task = self
            .project
            .tasks
            .iter_mut()
            .find(|t| t.branch == card.0)
            .ok_or_else(|| anyhow!("no card with branch '{}'", card.0))?;
        task.column = Some(col);
        Ok(())
    }

    fn comment(&mut self, card: &CardId, body: &str) -> Result<()> {
        let task = self
            .project
            .tasks
            .iter_mut()
            .find(|t| t.branch == card.0)
            .ok_or_else(|| anyhow!("no card with branch '{}'", card.0))?;
        task.notes.push(body.to_string());
        Ok(())
    }

    fn link(&mut self, card: &CardId, kind: Link, url: &str) -> Result<()> {
        let task = self
            .project
            .tasks
            .iter_mut()
            .find(|t| t.branch == card.0)
            .ok_or_else(|| anyhow!("no card with branch '{}'", card.0))?;
        task.links.push(TaskLink {
            kind,
            url: url.to_string(),
        });
        Ok(())
    }
}
