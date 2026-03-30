use serde::{Deserialize, Serialize};

use crate::work::{
    CreateChoreInput, CreateProductInput, CreateProjectInput, CreateTaskInput, Product, Project,
    Task, WorkItem, WorkItemPatch,
};

pub const TOPIC_WORK_PRODUCTS: &str = "work.products";

pub fn work_product_topic(product_id: &str) -> String {
    format!("work.product.{product_id}")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrontendRequestEnvelope {
    pub request_id: String,
    pub payload: FrontendRequest,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrontendEventEnvelope {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    pub payload: FrontendEvent,
}

impl FrontendEventEnvelope {
    pub fn response(request_id: impl Into<String>, payload: FrontendEvent) -> Self {
        Self {
            request_id: Some(request_id.into()),
            payload,
        }
    }

    pub fn push(payload: FrontendEvent) -> Self {
        Self {
            request_id: None,
            payload,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FrontendRequest {
    Subscribe {
        topics: Vec<String>,
    },
    Unsubscribe {
        topics: Vec<String>,
    },
    CreateProduct {
        #[serde(flatten)]
        input: CreateProductInput,
    },
    ListProducts,
    ListProjects {
        product_id: String,
    },
    ListTasks {
        product_id: String,
        project_id: Option<String>,
    },
    ListChores {
        product_id: String,
    },
    GetWorkItem {
        id: String,
    },
    CreateProject {
        #[serde(flatten)]
        input: CreateProjectInput,
    },
    CreateTask {
        #[serde(flatten)]
        input: CreateTaskInput,
    },
    CreateChore {
        #[serde(flatten)]
        input: CreateChoreInput,
    },
    UpdateWorkItem {
        id: String,
        patch: WorkItemPatch,
    },
    DeleteWorkItem {
        id: String,
    },
    GetWorkTree {
        product_id: String,
    },
    ReorderProjectTasks {
        project_id: String,
        task_ids: Vec<String>,
    },
    CreateAgent {
        name: Option<String>,
    },
    ListAgents,
    RemoveAgent {
        agent_id: String,
    },
    Prompt {
        agent_id: String,
        text: String,
    },
    PermissionResponse {
        agent_id: String,
        id: String,
        granted: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FrontendEvent {
    Hello {
        session_id: String,
    },
    Subscribed {
        topics: Vec<String>,
        current_revision: u64,
    },
    Unsubscribed {
        topics: Vec<String>,
    },
    TopicEvent {
        topic: String,
        revision: u64,
        origin_session_id: String,
        origin_request_id: Option<String>,
        event: TopicEventPayload,
    },
    ProductsList {
        products: Vec<Product>,
    },
    ProjectsList {
        product_id: String,
        projects: Vec<Project>,
    },
    TasksList {
        product_id: String,
        project_id: Option<String>,
        tasks: Vec<Task>,
    },
    ChoresList {
        product_id: String,
        chores: Vec<Task>,
    },
    WorkTree {
        product: Product,
        projects: Vec<Project>,
        tasks: Vec<Task>,
        chores: Vec<Task>,
    },
    WorkItemResult {
        item: WorkItem,
    },
    WorkItemCreated {
        item: WorkItem,
    },
    WorkItemUpdated {
        item: WorkItem,
    },
    ProjectTasksReordered {
        project_id: String,
        task_ids: Vec<String>,
    },
    WorkItemDeleted {
        id: String,
    },
    WorkError {
        message: String,
    },
    AgentCreated {
        agent_id: String,
        name: String,
    },
    AgentReady {
        agent_id: String,
    },
    AgentList {
        agents: Vec<AgentInfo>,
    },
    AgentRemoved {
        agent_id: String,
    },
    Chunk {
        agent_id: String,
        text: String,
    },
    Done {
        agent_id: String,
        stop_reason: String,
    },
    ToolCall {
        agent_id: String,
        name: String,
        status: String,
    },
    TerminalStarted {
        agent_id: String,
        id: String,
        title: String,
        command: String,
        cwd: Option<String>,
    },
    TerminalOutput {
        agent_id: String,
        id: String,
        text: String,
    },
    TerminalDone {
        agent_id: String,
        id: String,
        exit_code: Option<i64>,
        signal: Option<String>,
    },
    PermissionRequest {
        agent_id: String,
        id: String,
        title: String,
    },
    Error {
        agent_id: Option<String>,
        message: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentInfo {
    pub agent_id: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TopicEventPayload {
    WorkInvalidated {
        reason: String,
        product_id: Option<String>,
        item_ids: Vec<String>,
    },
}
