mod eval;
mod get_exercise;
mod git_commands;
mod headless_assistant;
mod judge;
mod templates_eval;

use clap::Parser;
use eval::{run_exercise_eval, save_eval_results};
use futures::stream::{self, StreamExt};
use get_exercise::{find_exercises, get_exercise_language, get_exercise_name};
use git_commands::read_base_sha;
use gpui::Application;
use headless_assistant::{authenticate_model_provider, find_model};
use language_model::LanguageModelRegistry;
use reqwest_client::ReqwestClient;
use std::{path::PathBuf, sync::Arc};
use templates_eval::all_templates;

#[derive(Parser, Debug)]
#[command(
    name = "assistant_eval",
    disable_version_flag = true,
    before_help = "Tool eval runner"
)]
struct Args {
    /// Regexes to match the names of evals to run.
    #[arg(long)]
    exercise_names: Vec<String>,
    /// Runs all exercises, causes the exercise_names to be ignored.
    #[arg(long)]
    all: bool,
    /// Supported language types to evaluate (default: python,go,rust,typescript,javascript,ruby,php,bash)
    #[arg(
        long,
        default_value = "python,go,rust,typescript,javascript,ruby,php,bash"
    )]
    languages: String,
    /// Name of the model (default: "claude-3-7-sonnet-latest")
    #[arg(long, default_value = "claude-3-7-sonnet-latest")]
    model_name: String,
    /// Name of the editor model (default: value of `--model_name`).
    #[arg(long)]
    editor_model_name: Option<String>,
    /// Name of the judge model (default: value of `--model_name`).
    #[arg(long)]
    judge_model_name: Option<String>,
    /// Number of evaluations to run concurrently (default: 3)
    #[arg(short, long, default_value = "3")]
    concurrency: usize,
    /// Maximum number of exercises to evaluate per language
    #[arg(long)]
    max_exercises_per_language: Option<usize>,
}

// First, let's define the order in which templates should be executed
const TEMPLATE_EXECUTION_ORDER: [&str; 3] = [
    "ProjectCreation",
    "CodeModification",
    "ConversationalGuidance",
];

fn main() {
    env_logger::init();
    let args = Args::parse();
    let http_client = Arc::new(ReqwestClient::new());
    let app = Application::headless().with_http_client(http_client.clone());

    // Path to the zed-ace-framework repo
    let framework_path = PathBuf::from("../zed-ace-framework")
        .canonicalize()
        .unwrap();

    // Fix the 'languages' lifetime issue by creating owned Strings instead of slices
    let languages: Vec<String> = args.languages.split(',').map(|s| s.to_string()).collect();

    println!("Using zed-ace-framework at: {:?}", framework_path);
    println!("Evaluating languages: {:?}", languages);

    app.run(move |cx| {
        let app_state = headless_assistant::init(cx);

        let model = find_model(&args.model_name, cx).unwrap();
        let editor_model = if let Some(model_name) = &args.editor_model_name {
            find_model(model_name, cx).unwrap()
        } else {
            model.clone()
        };
        let judge_model = if let Some(model_name) = &args.judge_model_name {
            find_model(model_name, cx).unwrap()
        } else {
            model.clone()
        };

        LanguageModelRegistry::global(cx).update(cx, |registry, cx| {
            registry.set_active_model(Some(model.clone()), cx);
            registry.set_editor_model(Some(editor_model.clone()), cx);
        });

        let model_provider_id = model.provider_id();
        let editor_model_provider_id = editor_model.provider_id();
        let judge_model_provider_id = judge_model.provider_id();

        let framework_path_clone = framework_path.clone();
        let languages_clone = languages.clone();
        let exercise_names = args.exercise_names.clone();
        let all_flag = args.all;

        cx.spawn(async move |cx| {
            // Authenticate all model providers first
            cx.update(|cx| authenticate_model_provider(model_provider_id.clone(), cx))
                .unwrap()
                .await
                .unwrap();
            cx.update(|cx| authenticate_model_provider(editor_model_provider_id.clone(), cx))
                .unwrap()
                .await
                .unwrap();
            cx.update(|cx| authenticate_model_provider(judge_model_provider_id.clone(), cx))
                .unwrap()
                .await
                .unwrap();

            // Read base SHA from setup.json
            let base_sha = read_base_sha(&framework_path_clone).await.unwrap();

            // Find all exercises for the specified languages
            let all_exercises = find_exercises(
                &framework_path_clone,
                &languages_clone
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>(),
                args.max_exercises_per_language,
            )
            .unwrap();
            println!("Found {} exercises total", all_exercises.len());

            // Filter exercises if specific ones were requested
            let exercises_to_run = if all_flag {
                all_exercises
            } else if !exercise_names.is_empty() {
                all_exercises
                    .into_iter()
                    .filter(|path| {
                        let name = get_exercise_name(path);
                        exercise_names.iter().any(|filter| name.contains(filter))
                    })
                    .collect()
            } else {
                all_exercises
            };

            println!("Will run {} exercises", exercises_to_run.len());

            // Get all templates and sort them according to the execution order
            let mut templates = all_templates();
            templates.sort_by_key(|template| {
                TEMPLATE_EXECUTION_ORDER
                    .iter()
                    .position(|&name| name == template.name)
                    .unwrap_or(usize::MAX)
            });

            // Create exercise eval tasks - each exercise is a single task that will run templates sequentially
            let exercise_tasks: Vec<_> = exercises_to_run
                .into_iter()
                .map(|exercise_path| {
                    let exercise_name = get_exercise_name(&exercise_path);
                    let templates_clone = templates.clone();
                    let model_clone = model.clone();
                    let judge_model_clone = judge_model.clone();
                    let app_state_clone = app_state.clone();
                    let base_sha_clone = base_sha.clone();
                    let framework_path_clone = framework_path_clone.clone();
                    let cx_clone = cx.clone();

                    async move {
                        println!("Processing exercise: {}", exercise_name);
                        let mut exercise_results = Vec::new();

                        // Determine the language for this exercise
                        let language = match get_exercise_language(&exercise_path) {
                            Ok(lang) => lang,
                            Err(err) => {
                                println!(
                                    "Error determining language for {}: {}",
                                    exercise_name, err
                                );
                                return exercise_results;
                            }
                        };

                        // Run each template sequentially for this exercise
                        for template in templates_clone {
                            // For "multi" language, only run the CodeModification template
                            if language == "multi" && template.name != "CodeModification" {
                                println!("Skipping {} template for multi language", template.name);
                                continue;
                            }

                            match run_exercise_eval(
                                exercise_path.clone(),
                                template.clone(),
                                model_clone.clone(),
                                judge_model_clone.clone(),
                                app_state_clone.clone(),
                                base_sha_clone.clone(),
                                framework_path_clone.clone(),
                                cx_clone.clone(),
                            )
                            .await
                            {
                                Ok(result) => {
                                    println!(
                                        "Completed {} with template {} - score: {}",
                                        exercise_name, template.name, result.score
                                    );
                                    exercise_results.push(result);
                                }
                                Err(err) => {
                                    println!(
                                        "Error running {} with template {}: {}",
                                        exercise_name, template.name, err
                                    );
                                }
                            }
                        }

                        // Save results for this exercise
                        if !exercise_results.is_empty() {
                            if let Err(err) =
                                save_eval_results(&exercise_path, exercise_results.clone()).await
                            {
                                println!("Error saving results for {}: {}", exercise_name, err);
                            } else {
                                println!("Saved results for {}", exercise_name);
                            }
                        }

                        exercise_results
                    }
                })
                .collect();

            println!(
                "Running {} exercises with concurrency: {}",
                exercise_tasks.len(),
                args.concurrency
            );

            // Run exercises concurrently, with each exercise running its templates sequentially
            let all_results = stream::iter(exercise_tasks)
                .buffer_unordered(args.concurrency)
                .flat_map(stream::iter)
                .collect::<Vec<_>>()
                .await;

            println!("Completed {} evaluation runs", all_results.len());
            cx.update(|cx| cx.quit()).unwrap();
        })
        .detach();
    });

    println!("Done running evals");
}
