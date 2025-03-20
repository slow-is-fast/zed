use crate::git_commands::{checkout_repo, query_git, run_git, run_git_command, setup_temp_repo};
use crate::headless_assistant::{HeadlessAppState, HeadlessAssistant};
use crate::{get_exercise_language, get_exercise_name, templates_eval::Template};
use anyhow::{anyhow, Result};
use assistant2::RequestKind;
use collections::HashMap;
use gpui::{App, Task};
use language_model::{LanguageModel, TokenUsage};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime},
};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct EvalResult {
    pub exercise_name: String,
    pub template_name: String,
    pub score: String,
    pub diff: String,
    pub assistant_response: String,
    pub elapsed_time_ms: u128,
    pub timestamp: u128,
    // Token usage fields
    pub input_tokens: usize,
    pub output_tokens: usize,
    pub total_tokens: usize,
    pub tool_use_counts: usize,
}

pub struct EvalOutput {
    pub diff: String,
    pub last_message: String,
    pub elapsed_time: Duration,
    pub assistant_response_count: usize,
    pub tool_use_counts: HashMap<Arc<str>, u32>,
    pub token_usage: TokenUsage,
}

#[derive(Deserialize)]
pub struct EvalSetup {
    pub url: String,
    pub base_sha: String,
}

pub struct Eval {
    pub repo_path: PathBuf,
    pub eval_setup: EvalSetup,
    pub user_prompt: String,
}

impl Eval {
    // Keep this method for potential future use, but mark it as intentionally unused
    #[allow(dead_code)]
    pub async fn load(_name: String, path: PathBuf, repos_dir: &Path) -> Result<Self> {
        let prompt_path = path.join("prompt.txt");
        let user_prompt = smol::unblock(|| std::fs::read_to_string(prompt_path)).await?;
        let setup_path = path.join("setup.json");
        let setup_contents = smol::unblock(|| std::fs::read_to_string(setup_path)).await?;
        let eval_setup = serde_json_lenient::from_str_lenient::<EvalSetup>(&setup_contents)?;

        // Move this internal function inside the load method since it's only used here
        fn repo_dir_name(url: &str) -> String {
            url.trim_start_matches("https://")
                .replace(|c: char| !c.is_alphanumeric(), "_")
        }

        let repo_path = repos_dir.join(repo_dir_name(&eval_setup.url));

        Ok(Eval {
            repo_path,
            eval_setup,
            user_prompt,
        })
    }

    pub fn run(
        self,
        app_state: Arc<HeadlessAppState>,
        model: Arc<dyn LanguageModel>,
        cx: &mut App,
    ) -> Task<Result<EvalOutput>> {
        cx.spawn(async move |cx| {
            checkout_repo(&self.repo_path, &self.eval_setup.base_sha).await?;

            let (assistant, done_rx) =
                cx.update(|cx| HeadlessAssistant::new(app_state.clone(), cx))??;

            let _worktree = assistant
                .update(cx, |assistant, cx| {
                    assistant.project.update(cx, |project, cx| {
                        project.create_worktree(&self.repo_path, true, cx)
                    })
                })?
                .await?;

            let start_time = std::time::SystemTime::now();

            let (system_prompt_context, load_error) = cx
                .update(|cx| {
                    assistant
                        .read(cx)
                        .thread
                        .read(cx)
                        .load_system_prompt_context(cx)
                })?
                .await;

            if let Some(load_error) = load_error {
                return Err(anyhow!("{:?}", load_error));
            };

            assistant.update(cx, |assistant, cx| {
                assistant.thread.update(cx, |thread, cx| {
                    let context = vec![];
                    thread.insert_user_message(self.user_prompt.clone(), context, None, cx);
                    thread.set_system_prompt_context(system_prompt_context);
                    thread.send_to_model(model, RequestKind::Chat, cx);
                });
            })?;

            done_rx.recv().await??;

            // Add this section to check untracked files
            println!("Checking for untracked files:");
            let untracked = query_git(
                &self.repo_path,
                &["ls-files", "--others", "--exclude-standard"],
            )
            .await?;
            if untracked.is_empty() {
                println!("No untracked files found");
            } else {
                // Add all files to git so they appear in the diff
                println!("Adding untracked files to git");
                run_git(&self.repo_path, &["add", "."]).await?;
            }

            // get git status
            let _status = query_git(&self.repo_path, &["status", "--short"]).await?;

            let elapsed_time = start_time.elapsed()?;

            // Get diff of staged changes (the files we just added)
            let staged_diff = query_git(&self.repo_path, &["diff", "--staged"]).await?;

            // Get diff of unstaged changes
            let unstaged_diff = query_git(&self.repo_path, &["diff"]).await?;

            // Combine both diffs
            let diff = if unstaged_diff.is_empty() {
                staged_diff
            } else if staged_diff.is_empty() {
                unstaged_diff
            } else {
                format!(
                    "# Staged changes\n{}\n\n# Unstaged changes\n{}",
                    staged_diff, unstaged_diff
                )
            };

            assistant.update(cx, |assistant, cx| {
                let thread = assistant.thread.read(cx);
                let last_message = thread.messages().last().unwrap();
                if last_message.role != language_model::Role::Assistant {
                    return Err(anyhow!("Last message is not from assistant"));
                }
                let assistant_response_count = thread
                    .messages()
                    .filter(|message| message.role == language_model::Role::Assistant)
                    .count();
                Ok(EvalOutput {
                    diff,
                    last_message: last_message.text.clone(),
                    elapsed_time,
                    assistant_response_count,
                    tool_use_counts: assistant.tool_use_counts.clone(),
                    token_usage: thread.cumulative_token_usage(),
                })
            })?
        })
    }
}

impl EvalOutput {
    // Keep this method for potential future use, but mark it as intentionally unused
    #[allow(dead_code)]
    pub fn save_to_directory(&self, output_dir: &Path, eval_output_value: String) -> Result<()> {
        // Create the output directory if it doesn't exist
        fs::create_dir_all(&output_dir)?;

        // Save the diff to a file
        let diff_path = output_dir.join("diff.patch");
        let mut diff_file = fs::File::create(&diff_path)?;
        diff_file.write_all(self.diff.as_bytes())?;

        // Save the last message to a file
        let message_path = output_dir.join("assistant_response.txt");
        let mut message_file = fs::File::create(&message_path)?;
        message_file.write_all(self.last_message.as_bytes())?;

        // Current metrics for this run
        let current_metrics = serde_json::json!({
            "elapsed_time_ms": self.elapsed_time.as_millis(),
            "assistant_response_count": self.assistant_response_count,
            "tool_use_counts": self.tool_use_counts,
            "token_usage": self.token_usage,
            "eval_output_value": eval_output_value,
        });

        // Get current timestamp in milliseconds
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_millis()
            .to_string();

        // Path to metrics file
        let metrics_path = output_dir.join("metrics.json");

        // Load existing metrics if the file exists, or create a new object
        let mut historical_metrics = if metrics_path.exists() {
            let metrics_content = fs::read_to_string(&metrics_path)?;
            serde_json::from_str::<serde_json::Value>(&metrics_content)
                .unwrap_or_else(|_| serde_json::json!({}))
        } else {
            serde_json::json!({})
        };

        // Add new run with timestamp as key
        if let serde_json::Value::Object(ref mut map) = historical_metrics {
            map.insert(timestamp, current_metrics);
        }

        // Write updated metrics back to file
        let metrics_json = serde_json::to_string_pretty(&historical_metrics)?;
        let mut metrics_file = fs::File::create(&metrics_path)?;
        metrics_file.write_all(metrics_json.as_bytes())?;

        Ok(())
    }
}

pub async fn read_instructions(exercise_path: &Path) -> Result<String> {
    let instructions_path = exercise_path.join(".docs").join("instructions.md");
    println!("Reading instructions from: {}", instructions_path.display());
    let instructions = smol::unblock(move || std::fs::read_to_string(&instructions_path)).await?;
    Ok(instructions)
}

pub async fn read_example_solution(exercise_path: &Path, language: &str) -> Result<String> {
    // Map the language to the file extension
    let language_extension = match language {
        "python" => "py",
        "go" => "go",
        "rust" => "rs",
        "typescript" => "ts",
        "javascript" => "js",
        "ruby" => "rb",
        "php" => "php",
        "bash" => "sh",
        "multi" => "diff",
        _ => return Err(anyhow!("Unsupported language: {}", language)),
    };
    let example_path = exercise_path
        .join(".meta")
        .join(format!("example.{}", language_extension));
    println!("Reading example solution from: {}", example_path.display());
    let example = smol::unblock(move || std::fs::read_to_string(&example_path)).await?;
    Ok(example)
}

pub async fn save_eval_results(exercise_path: &Path, results: Vec<EvalResult>) -> Result<()> {
    let eval_dir = exercise_path.join("evaluation");
    fs::create_dir_all(&eval_dir)?;

    let eval_file = eval_dir.join("evals.json");

    println!("Saving evaluation results to: {}", eval_file.display());
    println!(
        "Results to save: {} evaluations for exercise path: {}",
        results.len(),
        exercise_path.display()
    );

    // Check file existence before reading/writing
    if eval_file.exists() {
        println!("Existing evals.json file found, will update it");
    } else {
        println!("No existing evals.json file found, will create new one");
    }

    // Structure to organize evaluations by test name and timestamp
    let mut eval_data: serde_json::Value = if eval_file.exists() {
        let content = fs::read_to_string(&eval_file)?;
        serde_json::from_str(&content).unwrap_or_else(|_| serde_json::json!({}))
    } else {
        serde_json::json!({})
    };

    // Get current timestamp for this batch of results
    let timestamp = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)?
        .as_millis()
        .to_string();

    // Group the new results by test name (exercise name)
    for result in results {
        let exercise_name = &result.exercise_name;
        let template_name = &result.template_name;

        println!(
            "Adding result: exercise={}, template={}",
            exercise_name, template_name
        );

        // Ensure the exercise entry exists
        if !eval_data.get(exercise_name).is_none() {
            eval_data[exercise_name] = serde_json::json!({});
        }

        // Ensure the timestamp entry exists as an object
        if !eval_data[exercise_name].get(&timestamp).is_none() {
            eval_data[exercise_name][&timestamp] = serde_json::json!({});
        }

        // Add this result under the timestamp with template name as key
        eval_data[exercise_name][&timestamp][template_name] = serde_json::to_value(&result)?;
    }

    // Write back to file with pretty formatting
    let json_content = serde_json::to_string_pretty(&eval_data)?;
    match fs::write(&eval_file, json_content) {
        Ok(_) => println!("✓ Successfully saved results to {}", eval_file.display()),
        Err(e) => println!("✗ Failed to write results file: {}", e),
    }

    Ok(())
}

pub async fn run_exercise_eval(
    exercise_path: PathBuf,
    template: Template,
    model: Arc<dyn LanguageModel>,
    judge_model: Arc<dyn LanguageModel>,
    app_state: Arc<HeadlessAppState>,
    base_sha: String,
    _framework_path: PathBuf,
    cx: gpui::AsyncApp,
) -> Result<EvalResult> {
    let exercise_name = get_exercise_name(&exercise_path);
    let language = get_exercise_language(&exercise_path)?;
    let mut instructions = read_instructions(&exercise_path).await?;
    instructions.push_str(&format!(
        "\n\nWhen writing the code for this prompt, use {} to achieve the goal.",
        language
    ));
    let example_solution = read_example_solution(&exercise_path, &language).await?;

    println!(
        "Running evaluation for exercise: {} with template: {}",
        exercise_name, template.name
    );

    // Create temporary directory with exercise files
    let temp_dir = setup_temp_repo(&exercise_path, &base_sha).await?;
    let temp_path = temp_dir.path().to_path_buf();

    if template.name == "ProjectCreation" {
        for entry in fs::read_dir(&temp_path)? {
            let entry = entry?;
            let path = entry.path();

            // Skip directories that start with dot (like .docs, .meta, .git)
            if path.is_dir()
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .map(|name| name.starts_with("."))
                    .unwrap_or(false)
            {
                continue;
            }

            // Delete regular files
            if path.is_file() {
                println!("  Deleting file: {}", path.display());
                fs::remove_file(path)?;
            }
        }

        // Commit the deletion so it shows up in the diff
        run_git_command(&temp_path, vec!["add", "."]).await?;
        run_git_command(
            &temp_path,
            vec!["commit", "-m", "Remove root files for clean slate"],
        )
        .await?;
    }

    let local_commit_sha = run_git_command(&temp_path, vec!["rev-parse", "HEAD"]).await?;

    // Prepare prompt based on template
    let prompt = match template.name {
        "ProjectCreation" => format!(
            "I need to create a new implementation for this exercise. Please create all the necessary files in the best location.\n\n{}",
            instructions
        ),
        "CodeModification" => format!(
            "I need help updating my code to meet these requirements. Please modify the appropriate files:\n\n{}",
            instructions
        ),
        "ConversationalGuidance" => format!(
            "I'm trying to solve this coding exercise but I'm not sure where to start. Can you help me understand the requirements and guide me through the solution process without writing code for me?\n\n{}",
            instructions
        ),
        _ => instructions.clone(),
    };

    let start_time = SystemTime::now();

    // Create a basic eval struct to work with the existing system
    let eval = Eval {
        repo_path: temp_path.clone(),
        eval_setup: EvalSetup {
            url: format!("file://{}", temp_path.display()),
            base_sha: local_commit_sha, // Use the local commit SHA instead of the framework base SHA
        },
        user_prompt: prompt,
    };

    // Run the evaluation
    let eval_output = cx
        .update(|cx| eval.run(app_state.clone(), model.clone(), cx))?
        .await?;

    // Get diff from git
    let diff = eval_output.diff.clone();

    // For project creation template, we need to compare with reference implementation
    let judge_output = if template.name == "ProjectCreation" {
        let project_judge_prompt = template
            .content
            .replace(
                "<!-- ```requirements go here``` -->",
                &format!("```\n{}\n```", instructions),
            )
            .replace(
                "<!-- ```reference code goes here``` -->",
                &format!("```{}\n{}\n```", language, example_solution),
            )
            .replace(
                "<!-- ```git diff goes here``` -->",
                &format!("```\n{}\n```", diff),
            );

        // Use the run_with_prompt method which we'll add to judge.rs
        let judge = crate::judge::Judge {
            original_diff: None,
            original_message: Some(project_judge_prompt),
            model: judge_model.clone(),
        };

        cx.update(|cx| judge.run_with_prompt(cx))?.await?
    } else if template.name == "CodeModification" {
        // For CodeModification, we'll compare the example solution with the LLM-generated solution
        let code_judge_prompt = template
            .content
            .replace(
                "<!-- ```reference code goes here``` -->",
                &format!("```{}\n{}\n```", language, example_solution),
            )
            .replace(
                "<!-- ```git diff goes here``` -->",
                &format!("```\n{}\n```", diff),
            );

        // Use the run_with_prompt method
        let judge = crate::judge::Judge {
            original_diff: None,
            original_message: Some(code_judge_prompt),
            model: judge_model.clone(),
        };

        cx.update(|cx| judge.run_with_prompt(cx))?.await?
    } else {
        // Conversational template
        let conv_judge_prompt = template
            .content
            .replace(
                "<!-- ```query goes here``` -->",
                &format!("```\n{}\n```", instructions),
            )
            .replace(
                "<!-- ```transcript goes here``` -->",
                &format!("```\n{}\n```", eval_output.last_message),
            )
            .replace(
                "<!-- ```git diff goes here``` -->",
                &format!("```\n{}\n```", diff),
            );

        // Use the run_with_prompt method for consistency
        let judge = crate::judge::Judge {
            original_diff: None,
            original_message: Some(conv_judge_prompt),
            model: judge_model.clone(),
        };

        cx.update(|cx| judge.run_with_prompt(cx))?.await?
    };

    let elapsed_time = start_time.elapsed()?;

    // Calculate total tokens as the sum of input and output tokens
    let input_tokens = eval_output.token_usage.input_tokens;
    let output_tokens = eval_output.token_usage.output_tokens;
    let tool_use_counts = eval_output.tool_use_counts.values().sum::<u32>();
    let total_tokens = input_tokens + output_tokens;

    // Save results to evaluation directory
    let result = EvalResult {
        exercise_name: exercise_name.clone(),
        template_name: template.name.to_string(),
        score: judge_output.trim().to_string(),
        diff,
        assistant_response: eval_output.last_message.clone(),
        elapsed_time_ms: elapsed_time.as_millis(),
        timestamp: SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)?
            .as_millis(),
        // Convert u32 token counts to usize
        input_tokens: input_tokens.try_into().unwrap(),
        output_tokens: output_tokens.try_into().unwrap(),
        total_tokens: total_tokens.try_into().unwrap(),
        tool_use_counts: tool_use_counts.try_into().unwrap(),
    };

    Ok(result)
}
