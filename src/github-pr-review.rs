use dotenv::dotenv;
use flowsnet_platform_sdk::write_error_log;
use github_flows::{
    get_octo, listen_to_event,
    octocrab::models::events::payload::{IssueCommentEventAction, IssuesEventAction},
    EventPayload,
};
use http_req::{
    request::{Method, Request},
    uri::Uri,
};
use openai_flows::{chat_completion, ChatModel, ChatOptions};
use std::env;

//  The soft character limit of the input context size
//   the max token size or word count for GPT4 is 8192
//   the max token size or word count for GPT35Turbo is 4096
static CHAR_SOFT_LIMIT : usize = 18000;
// static MODEL : ChatModel = ChatModel::GPT35Turbo;
static MODEL : ChatModel = ChatModel::GPT4;

#[no_mangle]
#[tokio::main(flavor = "current_thread")]
pub async fn run() -> anyhow::Result<()> {
    dotenv().ok();

    let login = env::var("login").unwrap_or("juntao".to_string());
    let owner = env::var("owner").unwrap_or("flows-network".to_string());
    let repo = env::var("repo").unwrap_or("review-any-pr-with-chatgpt".to_string());
    let trigger_phrase = env::var("trigger_phrase").unwrap_or("flows review".to_string());

    listen_to_event(
        &login,
        &owner,
        &repo,
        vec!["issue_comment", "issues"],
        |payload| handler(&login, &owner, &repo, &trigger_phrase, payload),
    )
    .await;

    Ok(())
}

async fn handler(
    login: &str,
    owner: &str,
    repo: &str,
    trigger_phrase: &str,
    payload: EventPayload,
) {
    let (pull_url, issue_number, _contributor) = match payload {
        EventPayload::IssuesEvent(e) => {
            if e.action != IssuesEventAction::Opened {
                // Only responds to newly opened issues
                write_error_log!("Received an ignorable event for issues.");
                return;
            }
            (e.issue.title, e.issue.number, e.issue.user.login)
        },

        EventPayload::IssueCommentEvent(e) => {
            if e.action == IssueCommentEventAction::Deleted {
                write_error_log!("Deleted issue event");
                return;
            }

            let body = e.comment.body.unwrap_or_default();

            // TODO: Makeshift but operational
            if body.starts_with("Hello, I am a [serverless review bot]") {
                write_error_log!("Ignore comment via bot");
                return;
            };

            if !body.to_lowercase().contains(&trigger_phrase.to_lowercase()) {
                write_error_log!(format!("Ignore the comment, raw: {}", body));
                return;
            }

            (e.issue.title, e.issue.number, e.issue.user.login)
        }
        _ => return,
    };

    let pull_url_components : Vec<&str> = pull_url.split("/").collect();
    if pull_url_components.len() < 5 { return; }
    let pull_number = pull_url_components[pull_url_components.len() - 1].parse::<u64>().unwrap();
    let pull_repo = pull_url_components[pull_url_components.len() - 3];
    let pull_owner = pull_url_components[pull_url_components.len() - 4];
    let chat_id = format!("PR#{pull_number}");
    let system = "You are a senior software developer experienced in code reviews.";

    let octo = get_octo(Some(String::from(login)));
    let pulls = octo.pulls(pull_owner, pull_repo);
    let mut resp = String::new();
    resp.push_str("Hello, I am a [serverless review bot](https://github.com/flows-network/github-pr-review/) on [flows.network](https://flows.network/). Here are my reviews of changed source code files in this PR.\n\n------\n\n");

    match pulls.list_files(pull_number).await {
        Ok(files) => {
            for f in files.items {
                let filename = &f.filename;
                if filename.ends_with(".md") || filename.ends_with(".js") || filename.ends_with(".css") || filename.ends_with(".html") || filename.ends_with(".htm") {
                    continue;
                }
                
                // The f.raw_url is a redirect. So, we need to construct our own here.
                let contents_url = f.contents_url.as_str();
                if contents_url.len() < 40 { continue; }
                let hash = &contents_url[(contents_url.len() - 40)..];
                let raw_url = format!(
                    "https://raw.githubusercontent.com/{pull_owner}/{pull_repo}/{}/{}", hash, filename
                );
                let file_uri = Uri::try_from(raw_url.as_str()).unwrap();
                let mut writer = Vec::new();
                match Request::new(&file_uri)
                    .method(Method::GET)
                    .header("Accept", "plain/text")
                    .header("User-Agent", "Flows Network Connector")
                    .send(&mut writer)
                    .map_err(|_e| {}) {
                        Err(_e) => {
                            write_error_log!("Cannot get file");
                            continue;
                        }
                        _ => {}
                }
                let file_as_text = String::from_utf8_lossy(&writer);
                let t_file_as_text = truncate(&file_as_text, CHAR_SOFT_LIMIT);

                resp.push_str("## [");
                resp.push_str(filename);
                resp.push_str("](");
                resp.push_str(f.blob_url.as_str());
                resp.push_str(")\n\n");

                let co = ChatOptions {
                    model: MODEL,
                    restart: true,
                    system_prompt: Some(system),
                    retry_times: 3,
                };
                let question = "Review the following source code and look for potential problems. The code might be truncated. So, do NOT comment on the completeness of the source code.\n\n".to_string() + t_file_as_text;
                if let Some(r) = chat_completion("gpt4", &chat_id, &question, &co) {
                    resp.push_str(&r.choice);
                    resp.push_str("\n\n");
                }

                let co = ChatOptions {
                    model: MODEL,
                    restart: false,
                    system_prompt: Some(system),
                    retry_times: 3,
                };
                let patch_as_text = f.patch.unwrap_or("".to_string());
                let t_patch_as_text = truncate(&patch_as_text, CHAR_SOFT_LIMIT);
                let question = "The following is a patch. Please summarize key changes.\n\n".to_string() + t_patch_as_text;
                if let Some(r) = chat_completion("gpt4", &chat_id, &question, &co) {
                    resp.push_str(&r.choice);
                    resp.push_str("\n\n");
                }
            }
            resp.push_str("cc ");
            resp.push_str(&pull_url);
            resp.push_str("\n");
        },
        Err(_error) => {
            write_error_log!("Cannot get file list");
        }
    }

    // Send the entire response to GitHub PR
    let issues = octo.issues(owner, repo);
    match issues.create_comment(issue_number, resp).await {
        Err(error) => {
            write_error_log!(format!("Error posting resp: {}", error));
        }
        _ => {}
    }
}

fn truncate(s: &str, max_chars: usize) -> &str {
    match s.char_indices().nth(max_chars) {
        None => s,
        Some((idx, _)) => &s[..idx],
    }
}
