//! The document, the sidebar and the page header around every dashboard page.

use topcoat::{
    Result,
    asset::{Asset, AssetConfig, asset},
    context::{Cx, app_context},
    router::query_params,
    view::{Child, View, component, view},
};

use super::BASE;
use crate::AppState;
use crate::routes::admin::{Project, all_projects};

const MARK: Asset = asset!("assets/dashboard/mark.svg");
const STYLES: Asset = topcoat::tailwind::stylesheet!();
const THEME_JS: Asset = asset!("assets/dashboard/theme.js");
const DASHBOARD_JS: Asset = asset!("assets/dashboard/dashboard.js");
const UPLOT_CSS: Asset = asset!("assets/dashboard/uplot.min.css");
const UPLOT_JS: Asset = asset!("assets/dashboard/uplot.min.js");
const CHART_JS: Asset = asset!("assets/dashboard/chart.js");

/// The browser tab icon (the brand mark on its accent tile).
const FAVICON: &str = "data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 40 40'%3E%3Crect width='40' height='40' rx='9' fill='%23c14d34'/%3E%3Cg transform='rotate(-14 20 20)'%3E%3Cmask id='m'%3E%3Crect width='40' height='40' fill='%23fff'/%3E%3Ccircle cx='7' cy='20' r='3.4'/%3E%3Ccircle cx='33' cy='20' r='3.4'/%3E%3C/mask%3E%3Crect x='7' y='11.5' width='26' height='17' rx='3' fill='%23fff9ee' mask='url(%23m)'/%3E%3Cg fill='%23c14d34'%3E%3Ccircle cx='25' cy='14.75' r='1.15'/%3E%3Ccircle cx='25' cy='18.25' r='1.15'/%3E%3Ccircle cx='25' cy='21.75' r='1.15'/%3E%3Ccircle cx='25' cy='25.25' r='1.15'/%3E%3C/g%3E%3C/g%3E%3C/svg%3E";

pub fn state(cx: &Cx) -> &AppState {
    app_context::<AppState>(cx)
}

/// The HTML document: head, styles and scripts.
#[component]
pub async fn document(cx: &Cx, title: &str, #[default] child: Child<'_>) -> Result<impl View> {
    let mark = app_context::<AssetConfig>(cx).resolve(MARK);
    Ok(view! {
        <!DOCTYPE html>
        <html lang="en" style=(format!("--brand-mark: url({mark})"))>
            <head>
                <meta charset="utf-8">
                <meta name="viewport" content="width=device-width, initial-scale=1">
                <meta name="color-scheme" content="light dark">
                <title>(title)</title>
                <link rel="icon" type="image/svg+xml" href=(FAVICON)>
                <link rel="stylesheet" href=(UPLOT_CSS)>
                <link rel="stylesheet" href=(STYLES)>
                <script src=(THEME_JS)></script>
                <script src=(DASHBOARD_JS) defer=(true)></script>
                <script src=(UPLOT_JS) defer=(true)></script>
                <script src=(CHART_JS) defer=(true)></script>
                topcoat::runtime::script()
                topcoat::dev::script()
            </head>
            <body>(child)</body>
        </html>
    })
}

#[derive(Clone, Copy, PartialEq)]
pub enum Section {
    Revenue,
    Customers,
    Setup,
}

impl Section {
    const ALL: [Section; 3] = [Section::Revenue, Section::Customers, Section::Setup];

    fn label(self) -> &'static str {
        match self {
            Section::Revenue => "Revenue",
            Section::Customers => "Customers",
            Section::Setup => "Setup",
        }
    }

    fn path(self) -> &'static str {
        match self {
            Section::Revenue => "/dashboard/",
            Section::Customers => "/dashboard/customers",
            Section::Setup => "/dashboard/setup",
        }
    }
}

/// `?project=<id>` on every page; the first project when absent or unknown.
#[query_params]
struct ProjectQuery {
    project: Option<String>,
}

/// All projects and the selected one.
pub struct Projects {
    pub all: Vec<Project>,
    pub current: Option<usize>,
}

impl Projects {
    pub async fn load(cx: &Cx) -> Result<Self> {
        let all = all_projects(&state(cx).db).await?;
        let wanted = query_params::<ProjectQuery>(cx)
            .ok()
            .and_then(|q| q.project.clone());
        let current = wanted
            .and_then(|id| all.iter().position(|p| p.id.to_string() == id))
            .or((!all.is_empty()).then_some(0));
        Ok(Self { all, current })
    }

    pub fn current(&self) -> Option<&Project> {
        self.current.map(|i| &self.all[i])
    }
}

/// A dashboard URL for the selected project.
pub fn href(path: &str, project: &Project) -> String {
    format!("{path}?project={}", project.id)
}

/// Sidebar, page header and content.
#[component]
pub async fn app(
    projects: &Projects,
    section: Section,
    #[default] child: Child<'_>,
) -> Result<impl View> {
    let project = projects.current();
    Ok(view! {
        document(
            title: "entitle dashboard",
            <div class="app">
                <aside class="sidebar">
                    <div class="brand">
                        <span class="brand-mark" aria-hidden="true"></span>
                        "entitle"
                    </div>
                    if let Some(project) = project {
                        <select
                            class="select select-sm w-full"
                            aria-label="Project"
                            data-navigate=""
                        >
                            for p in &projects.all {
                                <option
                                    value=(href(section.path(), p))
                                    selected=(p.id == project.id)
                                >
                                    (&p.name)
                                </option>
                            }
                            <option value=(format!("{BASE}/projects/new"))>
                                "+ New project…"
                            </option>
                        </select>
                        <nav class="nav" aria-label="View">
                            for s in Section::ALL {
                                <a
                                    class="nav-link"
                                    href=(href(s.path(), project))
                                    aria-current=((s == section).then_some("page"))
                                >
                                    icon(section: s)
                                    (s.label())
                                </a>
                            }
                        </nav>
                    }
                    <div class="sidebar-foot">
                        <button
                            type="button"
                            class="nav-foot nav-link"
                            data-theme-toggle=""
                            aria-label="Switch theme"
                        >
                            <svg
                                class="icon"
                                viewBox="0 0 24 24"
                                aria-hidden="true"
                                data-when-theme="dark"
                            >
                                <path d="M12 3a7 7 0 1 0 9 9 9 9 0 1 1-9-9Z"></path>
                            </svg>
                            <svg
                                class="icon"
                                viewBox="0 0 24 24"
                                aria-hidden="true"
                                data-when-theme="light"
                            >
                                <circle cx="12" cy="12" r="4"></circle>
                                <path
                                    d="M12 2v2M12 20v2M2 12h2M20 12h2M4.9 4.9l1.4 1.4M17.7 17.7l1.4 1.4M4.9 19.1l1.4-1.4M17.7 6.3l1.4-1.4"
                                ></path>
                            </svg>
                            <span data-when-theme="dark">"Dark"</span>
                            <span data-when-theme="light">"Light"</span>
                        </button>
                        <form method="post" action="/dashboard/logout">
                            <button type="submit" class="nav-foot nav-link">
                                <svg class="icon" viewBox="0 0 24 24" aria-hidden="true">
                                    <path d="M15 4h4v16h-4M10 8l-4 4 4 4M6 12h10"></path>
                                </svg>
                                "Sign out"
                            </button>
                        </form>
                    </div>
                </aside>
                <main class="content">
                    if let Some(project) = project {
                        <header class="page-head">
                            <h1>(section.label())</h1>
                            <span class="muted">(&project.name)</span>
                        </header>
                    }
                    if project.is_some() {
                        (child)
                    } else {
                        super::setup::new_project_form(cancel: None, error: None)
                    }
                </main>
            </div>
        )
    })
}

#[component]
async fn icon(section: Section) -> Result<impl View> {
    Ok(view! {
        <svg class="icon" viewBox="0 0 24 24" aria-hidden="true">
            match section {
                Section::Revenue => {
                    <path d="M4 19V10M10 19V5M16 19v-6M22 19H2"></path>
                }
                Section::Customers => {
                    <circle cx="9" cy="8" r="3.5"></circle>
                    <path
                        d="M2.5 19c.8-3.4 3.3-5 6.5-5s5.7 1.6 6.5 5M16 4.5a3.5 3.5 0 0 1 0 7M18.5 14c1.6.7 2.6 2.4 3 5"
                    ></path>
                }
                Section::Setup => {
                    <circle cx="12" cy="12" r="3"></circle>
                    <path
                        d="M12 2v3M12 19v3M4.9 4.9l2.1 2.1M17 17l2.1 2.1M2 12h3M19 12h3M4.9 19.1 7 17M17 7l2.1-2.1"
                    ></path>
                }
            }
        </svg>
    })
}
