use serde::{Deserialize, Serialize};

use crate::work::{
    CreateChoreInput, CreateProductInput, CreateProjectInput, CreateTaskInput, Product, Project,
    Task, WorkItem, WorkItemPatch,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FrontendRequest {
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
