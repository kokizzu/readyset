use readyset_errors::ReadySetResult;

use crate::query::MirQuery;

mod add_bogokey;
mod decorrelate;
mod filters_to_join_keys;
mod fuse;
mod predicate_pushup;
mod pull_columns;
mod pull_keys;

impl MirQuery<'_> {
    /// Run a set of rewrite and optimization passes on this [`MirQuery`], and returns the modified
    /// query.
    pub fn rewrite(mut self) -> ReadySetResult<Self> {
        pull_keys::pull_view_keys_to_leaf(&mut self)?;
        decorrelate::eliminate_dependent_joins(&mut self)?;
        predicate_pushup::push_filters_up(&mut self)?;
        filters_to_join_keys::convert_filters_to_join_keys(&mut self)?;
        add_bogokey::add_bogokey_if_necessary(&mut self)?;
        pull_columns::pull_all_required_columns(&mut self)?;
        fuse::fuse_project_nodes(&mut self)?;
        fuse::fuse_filter_nodes(&mut self)?;
        Ok(self)
    }
}
