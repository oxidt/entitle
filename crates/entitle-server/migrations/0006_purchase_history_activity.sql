-- Admin activity feed: newest recorded transitions per project and environment.
CREATE INDEX purchase_history_project_env_id ON purchase_history (project_id, environment, id DESC);
