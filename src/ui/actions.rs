//! The human's actions (spec §12): every POST has passed the session's CSRF check in
//! `require_session`, runs one service call as `Actor::Human` (which records a task event or
//! a message and publishes it like an agent's write) and answers with a 303 back to a page.

use axum::Form;
use axum::extract::{Path, RawForm, State};
use axum::response::{IntoResponse, Redirect, Response};
use serde::Deserialize;

use super::views::{UiError, UiResult};
use crate::domain::{
    Actor, Address, Importance, LinkKind, Message, MessageId, MessageKind, ProjectId, TaskId,
    TokenId, WakeTrigger, clean_display_name,
};
use crate::events::Event;
use crate::http::AppState;
use crate::service::auth::AuthError;
use crate::service::messages::{MessageError, NewMessage};
use crate::service::tasks::{Caller, NewTask, TaskError};
use crate::store::StoreError;

fn see_other(to: &str) -> Response {
    Redirect::to(to).into_response()
}

fn message_error(e: MessageError) -> UiError {
    match e {
        MessageError::BodyTooLarge { limit } => {
            UiError::Invalid(format!("The message is over {limit} bytes."))
        }
        e @ MessageError::TooLong { .. } => UiError::Invalid(capitalise(&e.to_string())),
        MessageError::NotFound(what) => UiError::Invalid(format!("That {what} does not exist.")),
        e @ (MessageError::MismatchedReply
        | MessageError::BadOptions(_)
        | MessageError::OptionsNeedHuman) => UiError::Invalid(capitalise(&e.to_string())),
        MessageError::Store(e) => e.into(),
    }
}

fn task_error(e: TaskError) -> UiError {
    match e {
        TaskError::NotFound => UiError::NotFound("No such task."),
        TaskError::Message(e) => message_error(e),
        TaskError::Store(e) => e.into(),
        e @ (TaskError::InvalidTransition { .. }
        | TaskError::Closed(_)
        | TaskError::NotOwner
        | TaskError::LeaseLost
        | TaskError::PlanNotApproved(_)
        | TaskError::NotPlanning(_)
        | TaskError::NoPlanToReview(_)
        | TaskError::PlanChanged
        | TaskError::DuplicateOfDone
        | TaskError::TooManyLinks(_)
        | TaskError::RequestChanged) => UiError::Conflict(capitalise(&e.to_string())),
        e @ (TaskError::SelfLink
        | TaskError::UnknownLinkTarget(_)
        | TaskError::DuplicateTwice
        | TaskError::OwnParent
        | TaskError::UnknownParent
        | TaskError::ParentIsDescendant) => UiError::Invalid(capitalise(&e.to_string())),
        e @ (TaskError::AlreadyAChild
        | TaskError::ParentClosed(_)
        | TaskError::MoveMakesAncestorWait { .. }) => UiError::Conflict(capitalise(&e.to_string())),
        e @ (TaskError::ReasonRequired
        | TaskError::NothingToUpdate
        | TaskError::WaitOnSelf
        | TaskError::WaitTargetNotFound
        | TaskError::WaitOnClosed(_)
        | TaskError::WaitOnAncestor
        | TaskError::WaitCycle(_)
        | TaskError::EmptyPlan
        | TaskError::NoteRequired
        | TaskError::UnknownProject
        | TaskError::InvalidLeaseTtl
        | TaskError::ChildExtraProjects
        | TaskError::ChildProjectMismatch
        | TaskError::InvalidField { .. }
        | TaskError::Guard(_)) => UiError::Invalid(capitalise(&e.to_string())),
    }
}

/// Service messages are lower-case fragments; pages show sentences.
fn capitalise(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => format!("{}{}.", c.to_uppercase(), chars.as_str()),
        None => String::new(),
    }
}

fn task_id(raw: &str) -> UiResult<TaskId> {
    raw.parse().map_err(|_| UiError::NotFound("No such task."))
}

/// An optional id field: blank is `None`, garbage is a 422.
fn opt_id<T: std::str::FromStr>(raw: &str, what: &str) -> UiResult<Option<T>> {
    match raw.trim() {
        "" => Ok(None),
        s => s
            .parse()
            .map(Some)
            .map_err(|_| UiError::Invalid(format!("Bad {what}."))),
    }
}

fn importance(raw: &str) -> UiResult<Importance> {
    match raw.trim() {
        "" => Ok(Importance::Normal),
        s => s
            .parse()
            .map_err(|_| UiError::Invalid("Importance is normal, high or urgent.".into())),
    }
}

fn required_body(body: &str) -> UiResult<()> {
    if body.trim().is_empty() {
        return Err(UiError::Invalid("A message needs some text.".into()));
    }
    Ok(())
}

fn subject(raw: &str) -> Option<String> {
    Some(raw.trim().to_string()).filter(|s| !s.is_empty())
}

// ---------------------------------------------------------------- tasks

/// The new-task form: `project` (the lead), repeated `also` (other participating projects,
/// top-level tasks only), `plan_first` and `start_now` (a ticked checkbox sends `on`; an
/// unticked one sends nothing), `title`, `body` and `parent`. Parsed by hand because a checkbox list repeats its field name.
#[derive(Default)]
struct CreateTaskForm {
    project: String,
    also: Vec<String>,
    plan_first: bool,
    start_now: bool,
    title: String,
    body: String,
    parent: String,
}

impl CreateTaskForm {
    fn parse(raw: &[u8]) -> UiResult<Self> {
        let fields: Vec<(String, String)> =
            serde_urlencoded::from_bytes(raw).map_err(|_| UiError::Invalid("Bad form.".into()))?;
        let mut form = CreateTaskForm::default();
        for (k, v) in fields {
            match k.as_str() {
                "project" => form.project = v,
                "also" => form.also.push(v),
                "plan_first" => form.plan_first = v == "on",
                "start_now" => form.start_now = v == "on",
                "title" => form.title = v,
                "body" => form.body = v,
                "parent" => form.parent = v,
                _ => {}
            }
        }
        Ok(form)
    }
}

pub async fn create_task(
    State(state): State<AppState>,
    RawForm(raw): RawForm,
) -> UiResult<Response> {
    let form = CreateTaskForm::parse(&raw)?;
    let title = form.title.trim();
    if title.is_empty() {
        return Err(UiError::Invalid("A task needs a title.".into()));
    }
    let project: ProjectId = opt_id(&form.project, "project")?
        .ok_or_else(|| UiError::Invalid("Pick a project.".into()))?;
    if state.store.get_project(project).await?.is_none() {
        return Err(UiError::Invalid("That project does not exist.".into()));
    }
    let parent = opt_id::<TaskId>(&form.parent, "parent task")?;
    let mut extra_projects = Vec::new();
    for raw in &form.also {
        let Some(id) = opt_id::<ProjectId>(raw, "project")? else {
            continue;
        };
        if state.store.get_project(id).await?.is_none() {
            return Err(UiError::Invalid("That project does not exist.".into()));
        }
        if id != project && !extra_projects.contains(&id) {
            extra_projects.push(id);
        }
    }
    let task = state
        .tasks
        .create(
            Caller::Human,
            NewTask {
                project_id: project,
                parent_id: parent,
                title: title.to_string(),
                body: form.body,
                extra_projects,
                requires_plan: form.plan_first,
                start_now: form.start_now,
                ..NewTask::default()
            },
        )
        .await
        .map_err(task_error)?;
    Ok(see_other(&format!("/ui/tasks/{}", task.id)))
}

#[derive(Deserialize)]
pub struct DecisionForm {
    #[serde(default)]
    note: String,
    /// The plan message the human read; the decision applies only while it is the latest.
    #[serde(default)]
    plan: String,
    /// The page to return to: the board or the overview (anything else: the task page).
    #[serde(default)]
    back: String,
}

/// Where a decision returns to: one of the pages that show "Needs you", else the task.
fn back_to(back: &str, id: TaskId) -> String {
    match back {
        "/ui/board" => "/ui/board#needs-you".into(),
        "/ui/" => "/ui/#needs-you".into(),
        _ => format!("/ui/tasks/{id}"),
    }
}

fn plan_id(raw: &str) -> UiResult<MessageId> {
    opt_id(raw, "plan")?.ok_or_else(|| UiError::Invalid("Which plan? Reload the page.".into()))
}

/// A decision on a plan that has since been replaced sends the human back to the task page,
/// which says so and shows the new plan.
fn decided(
    result: Result<crate::domain::Task, TaskError>,
    back: &str,
    id: TaskId,
) -> UiResult<Response> {
    match result {
        Ok(_) => Ok(see_other(&back_to(back, id))),
        Err(TaskError::PlanChanged) => {
            Ok(see_other(&format!("/ui/tasks/{id}?plan_changed=1#plan")))
        }
        Err(e) => Err(task_error(e)),
    }
}

/// The human approves the submitted plan they read; the task becomes `claimed`.
pub async fn approve_plan(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Form(form): Form<DecisionForm>,
) -> UiResult<Response> {
    let id = task_id(&id)?;
    let plan = plan_id(&form.plan)?;
    decided(state.tasks.approve_plan(id, plan).await, &form.back, id)
}

/// The human asks for changes to the submitted plan (a note is required).
pub async fn request_plan_changes(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Form(form): Form<DecisionForm>,
) -> UiResult<Response> {
    let id = task_id(&id)?;
    let plan = plan_id(&form.plan)?;
    decided(
        state.tasks.request_plan_changes(id, plan, &form.note).await,
        &form.back,
        id,
    )
}

#[derive(Deserialize)]
pub struct RequestForm {
    /// The task version the human read; the decision applies only while it is current.
    #[serde(default)]
    version: i64,
    #[serde(default)]
    note: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    back: String,
}

/// A decision on a request that has since changed says so (409) instead of acting on it.
fn request_error(e: TaskError) -> UiError {
    match e {
        TaskError::RequestChanged => {
            UiError::Conflict("Nothing was done: this request changed — review it again.".into())
        }
        e => task_error(e),
    }
}

/// The human approves work request `id` as they read it; it is routed or queued.
pub async fn approve_request(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Form(form): Form<RequestForm>,
) -> UiResult<Response> {
    let id = task_id(&id)?;
    state
        .tasks
        .approve_request(id, form.version)
        .await
        .map_err(request_error)?;
    Ok(see_other(&back_to(&form.back, id)))
}

/// The human declines work request `id`, with an optional note to the requester.
pub async fn decline_request(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Form(form): Form<RequestForm>,
) -> UiResult<Response> {
    let id = task_id(&id)?;
    state
        .tasks
        .decline_request(id, form.version, Some(&form.note))
        .await
        .map_err(request_error)?;
    Ok(see_other(&back_to(&form.back, id)))
}

/// The human answers the requester in the request's thread. The request stays pending; the
/// requester's agent is woken to read the answer. (A requested task has no owner, so the
/// thread post itself wakes nobody.)
pub async fn reply_request(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Form(form): Form<RequestForm>,
) -> UiResult<Response> {
    let id = task_id(&id)?;
    required_body(&form.body)?;
    let task = state
        .store
        .get_task(id)
        .await?
        .ok_or(UiError::NotFound("No such task."))?;
    state
        .messages
        .send_as_human(NewMessage {
            to: Address::Task(id),
            subject: None,
            body: form.body,
            importance: Importance::Normal,
            reply_to: None,
        })
        .await
        .map_err(message_error)?;
    if let Some(requester) = task.created_by.ids().0 {
        // The reply is saved; a failed wake is logged, never shown as a failed reply.
        if let Err(e) = state
            .wake
            .wake_agent(requester, WakeTrigger::HumanReply)
            .await
        {
            eprintln!("axl: failed to wake the requester of request {id}: {e}");
        }
    }
    Ok(see_other(&back_to(&form.back, id)))
}

#[derive(Deserialize)]
pub struct AddProjectForm {
    #[serde(default)]
    project: String,
}

/// Adds a participating project to a top-level task.
pub async fn add_project(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Form(form): Form<AddProjectForm>,
) -> UiResult<Response> {
    let id = task_id(&id)?;
    let project: ProjectId = opt_id(&form.project, "project")?
        .ok_or_else(|| UiError::Invalid("Pick a project.".into()))?;
    state
        .tasks
        .add_projects(Caller::Human, id, &[project])
        .await
        .map_err(task_error)?;
    Ok(see_other(&format!("/ui/tasks/{id}")))
}

#[derive(Deserialize)]
pub struct RelatedForm {
    /// The other task: its id, or a link to its page.
    #[serde(default)]
    task: String,
    /// `relates`, `follow-up` or `duplicate`.
    #[serde(default)]
    kind: String,
}

/// Links the task to another one as the human.
pub async fn add_related(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Form(form): Form<RelatedForm>,
) -> UiResult<Response> {
    let id = task_id(&id)?;
    let target = pasted_task(&form.task)?;
    let kind: LinkKind = form
        .kind
        .trim()
        .parse()
        .map_err(|_| UiError::Invalid("A link is relates, follow-up or duplicate.".into()))?;
    state
        .tasks
        .relate(Caller::Human, id, &[(target, kind)], &[])
        .await
        .map_err(task_error)?;
    Ok(see_other(&format!("/ui/tasks/{id}")))
}

/// A task id typed or pasted by the human: a bare id or a link to its page.
fn pasted_task(raw: &str) -> UiResult<TaskId> {
    let raw = raw.trim().trim_end_matches('/');
    let raw = raw.split(['?', '#']).next().unwrap_or_default();
    let raw = raw.rsplit('/').next().unwrap_or_default();
    opt_id(raw, "task id")?
        .ok_or_else(|| UiError::Invalid("Give the other task's id or link.".into()))
}

#[derive(Deserialize)]
pub struct ParentForm {
    /// The new parent: its id, or a link to its page.
    #[serde(default)]
    parent: String,
}

/// Moves a top-level task under a parent as the human.
pub async fn move_under(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Form(form): Form<ParentForm>,
) -> UiResult<Response> {
    let id = task_id(&id)?;
    let parent = pasted_task(&form.parent)?;
    state
        .tasks
        .move_under(Caller::Human, id, parent)
        .await
        .map_err(task_error)?;
    Ok(see_other(&format!("/ui/tasks/{id}")))
}

/// Removes the link between the task and `other`, whichever task added it.
pub async fn remove_related(
    State(state): State<AppState>,
    Path((id, other)): Path<(String, String)>,
) -> UiResult<Response> {
    let id = task_id(&id)?;
    let other = task_id(&other)?;
    state
        .tasks
        .relate(Caller::Human, id, &[], &[other])
        .await
        .map_err(task_error)?;
    Ok(see_other(&format!("/ui/tasks/{id}")))
}

#[derive(Deserialize)]
pub struct PostForm {
    #[serde(default)]
    body: String,
    #[serde(default)]
    subject: String,
    #[serde(default)]
    importance: String,
    /// The page to return to: the board or the overview (a "Blocked on you" reply), else the
    /// task page.
    #[serde(default)]
    back: String,
}

/// Posts into the task's thread as the human.
pub async fn post_to_thread(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Form(form): Form<PostForm>,
) -> UiResult<Response> {
    let id = task_id(&id)?;
    required_body(&form.body)?;
    if state.store.get_task(id).await?.is_none() {
        return Err(UiError::NotFound("No such task."));
    }
    state
        .messages
        .send(
            &Actor::Human,
            NewMessage {
                to: Address::Task(id),
                subject: subject(&form.subject),
                body: form.body,
                importance: importance(&form.importance)?,
                reply_to: None,
            },
        )
        .await
        .map_err(message_error)?;
    Ok(see_other(&back_to(&form.back, id)))
}

pub async fn force_release(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> UiResult<Response> {
    let id = task_id(&id)?;
    state.tasks.force_release(id).await.map_err(task_error)?;
    Ok(see_other(&format!("/ui/tasks/{id}")))
}

pub async fn cancel(State(state): State<AppState>, Path(id): Path<String>) -> UiResult<Response> {
    let id = task_id(&id)?;
    state
        .tasks
        .cancel(Caller::Human, id)
        .await
        .map_err(task_error)?;
    Ok(see_other(&format!("/ui/tasks/{id}")))
}

// ---------------------------------------------------------------- ack

#[derive(Deserialize)]
pub struct AckForm {
    /// The page to return to (see `ack_back`).
    #[serde(default)]
    back: String,
}

/// Where an ack returns to: the board or History (with any query), else the board. `//`,
/// `/\`, absolute URLs, non-ASCII text and other UI pages all fall back, so the redirect never leaves the UI.
fn ack_back(back: &str) -> &str {
    let ok = |page: &str| {
        back == page
            || back
                .strip_prefix(page)
                .is_some_and(|rest| rest.starts_with('?'))
    };
    if (ok("/ui/board") || ok("/ui/history"))
        && back.is_ascii()
        && !back.chars().any(char::is_control)
    {
        back
    } else {
        "/ui/board"
    }
}

/// `POST /ui/tasks/{id}/ack`: marks one finished task as seen. An unknown or not-done id is
/// a no-op.
pub async fn ack_task(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Form(form): Form<AckForm>,
) -> UiResult<Response> {
    let id = task_id(&id)?;
    state.tasks.ack_done(&[id]).await.map_err(task_error)?;
    Ok(see_other(ack_back(&form.back)))
}

/// `POST /ui/done/ack` (`csrf`, `back`, repeated `id`): marks exactly the listed finished
/// tasks as seen (the board's Ack all and History's Ack these). One unparsable id refuses the
/// whole batch.
pub async fn ack_done(State(state): State<AppState>, RawForm(raw): RawForm) -> UiResult<Response> {
    let fields: Vec<(String, String)> =
        serde_urlencoded::from_bytes(&raw).map_err(|_| UiError::Invalid("Bad form.".into()))?;
    let mut ids = Vec::new();
    let mut back = "";
    for (k, v) in &fields {
        match k.as_str() {
            "id" => ids.push(
                v.parse::<TaskId>()
                    .map_err(|_| UiError::Invalid("Bad task id.".into()))?,
            ),
            "back" => back = v,
            _ => {}
        }
    }
    if !ids.is_empty() {
        state.tasks.ack_done(&ids).await.map_err(task_error)?;
    }
    Ok(see_other(ack_back(back)))
}

// ---------------------------------------------------------------- messages

#[derive(Deserialize)]
pub struct MessageForm {
    /// `project`, `agent` or `task`; may instead prefix `to` as `kind:id`.
    #[serde(default)]
    to_kind: String,
    #[serde(default)]
    to: String,
    #[serde(default)]
    subject: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    importance: String,
    #[serde(default)]
    reply_to: String,
    /// The quick reply clicked, if any: it leads the body, before any typed text.
    #[serde(default)]
    option: String,
    /// A Needs-you card's page (`/ui/` or `/ui/board`) to return to.
    #[serde(default)]
    back: String,
}

/// The reply body: a clicked quick reply, then (after a blank line) whatever was typed.
fn reply_body(option: &str, typed: String) -> String {
    match (option.trim(), typed.trim().is_empty()) {
        ("", _) => typed,
        (option, true) => option.to_string(),
        (option, false) => format!("{option}\n\n{typed}"),
    }
}

fn address(kind: &str, id: &str) -> UiResult<Address> {
    let bad = || UiError::Invalid("Pick who the message is for.".into());
    let id: uuid::Uuid = id.trim().parse().map_err(|_| bad())?;
    Ok(match kind.trim() {
        "project" => Address::Project(id.into()),
        "agent" => Address::Agent(id.into()),
        "task" => Address::Task(id.into()),
        _ => return Err(bad()),
    })
}

/// Sends a message as the human to a project, an agent or a task thread.
pub async fn send_message(
    State(state): State<AppState>,
    Form(form): Form<MessageForm>,
) -> UiResult<Response> {
    let body = reply_body(&form.option, form.body);
    required_body(&body)?;
    let to = match form.to_kind.trim() {
        "" => {
            let (kind, id) = form.to.split_once(':').unwrap_or(("", ""));
            address(kind, id)?
        }
        kind => address(kind, &form.to)?,
    };
    let reply_to = opt_id::<MessageId>(&form.reply_to, "message id")?;
    // A reply to the human's mail archives the human's copy of it in the same write.
    let sent = state
        .messages
        .send_as_human(NewMessage {
            to,
            subject: subject(&form.subject),
            body,
            importance: importance(&form.importance)?,
            reply_to,
        })
        .await
        .map_err(message_error)?;
    Ok(see_other(&match (form.back.as_str(), reply_to) {
        ("/ui/", Some(_)) => "/ui/#needs-you".to_string(),
        ("/ui/board", Some(_)) => "/ui/board#needs-you".to_string(),
        (_, Some(original)) => format!("/ui/inbox?m={original}"),
        (_, None) => format!("/ui/inbox?view=all&m={}", sent.id),
    }))
}

#[derive(Deserialize)]
pub struct DismissForm {
    #[serde(default)]
    back: String,
}

/// Marks a message handled without answering it: archived for the human only, then back
/// to the page that showed it.
pub async fn dismiss(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Form(form): Form<DismissForm>,
) -> UiResult<Response> {
    let id: MessageId = id
        .parse()
        .map_err(|_| UiError::NotFound("No such message."))?;
    if state.store.get_message(id).await?.is_none() {
        return Err(UiError::NotFound("No such message."));
    }
    state
        .messages
        .ack(Caller::Human, &[id])
        .await
        .map_err(message_error)?;
    Ok(see_other(match form.back.as_str() {
        "/ui/" => "/ui/#needs-you",
        "/ui/inbox" => "/ui/inbox",
        _ => "/ui/board#needs-you",
    }))
}

/// Acks a message in the human inbox ("archive" hides it; nothing is deleted).
pub async fn archive(State(state): State<AppState>, Path(id): Path<String>) -> UiResult<Response> {
    let id: MessageId = id
        .parse()
        .map_err(|_| UiError::NotFound("No such message."))?;
    state
        .messages
        .ack(Caller::Human, &[id])
        .await
        .map_err(message_error)?;
    Ok(see_other("/ui/inbox"))
}

// ---------------------------------------------------------------- projects and tokens

#[derive(Deserialize)]
pub struct RenameForm {
    #[serde(default)]
    display_name: String,
}

pub async fn rename_project(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Form(form): Form<RenameForm>,
) -> UiResult<Response> {
    let id: ProjectId = id
        .parse()
        .map_err(|_| UiError::NotFound("No such project."))?;
    let name = clean_display_name(&form.display_name).map_err(|e| UiError::Invalid(e.into()))?;
    match state.store.rename_project(id, &name).await {
        Ok(()) => Ok(see_other("/ui/projects")),
        Err(StoreError::NotFound(_)) => Err(UiError::NotFound("No such project.")),
        Err(e) => Err(e.into()),
    }
}

/// Writes a thread-less system note to the human's inbox (and tells the live UI).
async fn human_note(state: &AppState, project: Option<ProjectId>, body: String) -> UiResult<()> {
    let message = Message {
        id: MessageId::new(),
        project_id: project,
        from: Actor::Human,
        to: Address::Human,
        subject: None,
        body,
        importance: Importance::Normal,
        task_id: None,
        reply_to: None,
        created_at: state.clock.now(),
        kind: MessageKind::System,
        options: Vec::new(),
    };
    state.store.insert_message(&message, None).await?;
    state
        .bus
        .emit(Event::MessageCreated {
            id: message.id,
            to: message.to,
            project_id: message.project_id,
        })
        .await;
    Ok(())
}

#[derive(Deserialize)]
pub struct AutoStartForm {
    /// A ticked checkbox sends `on`; an unticked one sends nothing.
    #[serde(default)]
    auto_start: String,
}

/// Turns a project's auto-start of peers' work requests on or off. A change writes a system
/// note; saving the current state again does not.
pub async fn set_auto_start(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Form(form): Form<AutoStartForm>,
) -> UiResult<Response> {
    let id: ProjectId = id
        .parse()
        .map_err(|_| UiError::NotFound("No such project."))?;
    let project = state
        .store
        .get_project(id)
        .await?
        .ok_or(UiError::NotFound("No such project."))?;
    let on = form.auto_start == "on";
    let mut settings = state.store.wake_settings(id).await?;
    if settings.auto_start != on {
        settings.auto_start = on;
        state
            .store
            .put_wake_settings(&settings, state.clock.now())
            .await?;
        let word = if on { "on" } else { "off" };
        human_note(
            &state,
            Some(id),
            format!("Auto-start turned {word} for {}", project.display_name),
        )
        .await?;
    }
    Ok(see_other("/ui/projects"))
}

#[derive(Deserialize)]
pub struct WakePauseForm {
    /// `on` pauses wake-ups; anything else resumes them.
    #[serde(default)]
    pause: String,
}

/// The wake-up kill switch. Resuming places the work that queued up meanwhile.
pub async fn set_wake_paused(
    State(state): State<AppState>,
    Form(form): Form<WakePauseForm>,
) -> UiResult<Response> {
    let pause = form.pause == "on";
    let mut settings = state.store.server_settings().await?;
    if settings.wake_paused != pause {
        settings.wake_paused = pause;
        state
            .store
            .put_server_settings(&settings, state.clock.now())
            .await?;
        let note = if pause {
            "Wake-ups paused"
        } else {
            "Wake-ups resumed"
        };
        human_note(&state, None, note.into()).await?;
        if !pause {
            // The settings are saved; a failed placement is logged, the ticker retries.
            if let Err(e) = state.wake.place_queued().await {
                eprintln!("axl: failed to place queued work after resuming wake-ups: {e}");
            }
        }
    }
    Ok(see_other("/ui/"))
}

pub async fn revoke_token(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> UiResult<Response> {
    let id: TokenId = id
        .parse()
        .map_err(|_| UiError::NotFound("No such token."))?;
    match state.auth.revoke(id).await {
        Ok(()) => Ok(see_other("/ui/tokens")),
        Err(AuthError::Store(StoreError::NotFound(_))) => Err(UiError::NotFound("No such token.")),
        Err(e) => Err(UiError::Internal(e.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_parse_by_kind() {
        let id = uuid::Uuid::now_v7();
        assert_eq!(
            address("project", &id.to_string()).unwrap(),
            Address::Project(id.into())
        );
        assert!(address("session", &id.to_string()).is_err());
        assert!(address("agent", "nope").is_err());
        assert_eq!(capitalise("cannot move"), "Cannot move.");
    }

    #[test]
    fn quick_reply_leads_the_typed_text() {
        assert_eq!(reply_body("", "typed".into()), "typed");
        assert_eq!(reply_body("Yes", " \n ".into()), "Yes");
        assert_eq!(reply_body(" Yes ", "and more".into()), "Yes\n\nand more");
    }
}
