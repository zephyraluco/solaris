//! Project Solaris questions onto ACP elicitation or a client's choice picker.
use super::session::{ClientConnection, call_id};
use agent_client_protocol::v1::schema::*;
use solaris_agent::{
    QuestionAnswer, QuestionRequest, QuestionResponse, RuntimeCommand, SessionRuntime,
};

pub(super) async fn answer(
    connection: ClientConnection,
    session: SessionRuntime,
    id: SessionId,
    run: Option<u64>,
    request: QuestionRequest,
    forms: bool,
) {
    let request_id = request.request_id.clone();
    let selection = async {
        if forms {
            form(&connection, &id, run, &request).await
        } else {
            choices(&connection, &id, run, &request).await
        }
    };
    let response = tokio::select! {
        _ = connection.incoming_closed() => return,
        _ = async {
            while session.is_question_pending(&request_id) {
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        } => return,
        response = selection => response.unwrap_or(QuestionResponse::Dismissed),
    };
    // Runtime validates the complete answer. A malformed client response must
    // never strand the waiting tool or let a partial selection reach the model.
    if let Ok(receipt) = session.dispatch(RuntimeCommand::AnswerQuestion {
        request_id: request_id.clone(),
        response,
    }) && receipt.accepted().await.is_err()
        && session.is_question_pending(&request_id)
    {
        let _ = session.dispatch(RuntimeCommand::AnswerQuestion {
            request_id,
            response: QuestionResponse::Dismissed,
        });
    }
}

async fn form(
    connection: &ClientConnection,
    id: &SessionId,
    run: Option<u64>,
    request: &QuestionRequest,
) -> Option<QuestionResponse> {
    let mut schema = ElicitationSchema::new().title("Solaris questions");
    for (index, question) in request.questions.iter().enumerate() {
        let options: Vec<_> = question
            .options
            .iter()
            .map(|option| {
                EnumOption::new(&option.id, &option.label).description(option.description.clone())
            })
            .collect();
        if !options.is_empty() {
            let key = format!("choice{index}");
            if question.multiple {
                schema = schema.property(
                    key,
                    MultiSelectPropertySchema::titled(options)
                        .title(question.header.clone())
                        .description(question.question.clone()),
                    !question.allow_custom,
                );
            } else {
                schema = schema.property(
                    key,
                    StringPropertySchema::new()
                        .one_of(options)
                        .title(question.header.clone())
                        .description(question.question.clone()),
                    !question.allow_custom,
                );
            }
        }
        if question.allow_custom {
            schema = schema.property(
                format!("custom{index}"),
                StringPropertySchema::new()
                    .title(format!("{}: custom answer", question.header))
                    .description(question.question.clone()),
                question.options.is_empty(),
            );
        }
    }
    let scope =
        ElicitationSessionScope::new(id.clone()).tool_call_id(call_id(run, &request.tool_call_id));
    let result = connection
        .send_request(CreateElicitationRequest::new(
            ElicitationFormMode::new(scope, schema),
            "Answer the agent's questions",
        ))
        .block_task()
        .await
        .ok()?;
    let ElicitationAction::Accept(accepted) = result.action else {
        return None;
    };
    let mut content = accepted.content?;
    let mut answers = Vec::with_capacity(request.questions.len());
    for (index, question) in request.questions.iter().enumerate() {
        let selected = match content.remove(&format!("choice{index}")) {
            Some(ElicitationContentValue::String(value)) if !question.multiple => vec![value],
            Some(ElicitationContentValue::StringArray(values)) if question.multiple => values,
            None => Vec::new(),
            _ => return None,
        };
        let custom = match content.remove(&format!("custom{index}")) {
            Some(ElicitationContentValue::String(value)) => {
                Some(value.trim().to_owned()).filter(|value| !value.is_empty())
            }
            None => None,
            _ => return None,
        };
        answers.push(QuestionAnswer {
            question_id: question.id.clone(),
            selected,
            custom,
        });
    }
    content
        .is_empty()
        .then_some(QuestionResponse::Answered { answers })
}

/// Older clients such as Paseo expose only ordered choice actions. Use one
/// prompt per question, repeating a multi-choice prompt until the user is done.
async fn choices(
    connection: &ClientConnection,
    id: &SessionId,
    run: Option<u64>,
    request: &QuestionRequest,
) -> Option<QuestionResponse> {
    let mut answers = Vec::with_capacity(request.questions.len());
    for (index, question) in request.questions.iter().enumerate() {
        if question.options.is_empty() {
            return None;
        }
        let mut selected = Vec::new();
        loop {
            let mut options: Vec<_> = question
                .options
                .iter()
                .enumerate()
                .filter(|(_, option)| !selected.contains(&option.id))
                .map(|(index, option)| {
                    let label = if option.description.is_empty() {
                        option.label.clone()
                    } else {
                        format!("{} — {}", option.label, option.description)
                    };
                    PermissionOption::new(
                        format!("choice{index}"),
                        label,
                        PermissionOptionKind::AllowOnce,
                    )
                })
                .collect();
            if question.multiple && !selected.is_empty() {
                options.push(PermissionOption::new(
                    "done",
                    "Done selecting",
                    PermissionOptionKind::AllowOnce,
                ));
            }
            // Equal action kinds make the choice explicit even in clients that
            // automatically approve ordinary allow/reject permission prompts.
            options.push(PermissionOption::new(
                "dismiss",
                "Dismiss question",
                PermissionOptionKind::AllowOnce,
            ));
            let title = if selected.is_empty() {
                question.question.clone()
            } else {
                let labels = question
                    .options
                    .iter()
                    .filter(|option| selected.contains(&option.id))
                    .map(|option| option.label.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("{} (Selected: {labels})", question.question)
            };
            let picker = ToolCallId::new(format!(
                "{}:question:{index}:{}",
                call_id(run, &request.tool_call_id).0,
                selected.len()
            ));
            let result = connection
                .send_request(RequestPermissionRequest::new(
                    id.clone(),
                    ToolCallUpdate::new(picker, ToolCallUpdateFields::new().title(title)),
                    options,
                ))
                .block_task()
                .await
                .ok()?;
            let RequestPermissionOutcome::Selected(choice) = result.outcome else {
                return None;
            };
            let choice = choice.option_id.0.as_ref();
            if choice == "done" && question.multiple && !selected.is_empty() {
                break;
            }
            let index: usize = choice.strip_prefix("choice")?.parse().ok()?;
            let option = question.options.get(index)?;
            if selected.contains(&option.id) {
                return None;
            }
            selected.push(option.id.clone());
            if !question.multiple || selected.len() == question.options.len() {
                break;
            }
        }
        answers.push(QuestionAnswer {
            question_id: question.id.clone(),
            selected,
            custom: None,
        });
    }
    Some(QuestionResponse::Answered { answers })
}
