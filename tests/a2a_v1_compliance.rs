//! A2A v1.0 wire-format compliance tests.

use rsclaw::a2a::types::TaskState;

#[test]
fn task_state_serializes_to_v1_screaming_snake() {
    let cases = [
        (TaskState::Unspecified,    "\"TASK_STATE_UNSPECIFIED\""),
        (TaskState::Submitted,      "\"TASK_STATE_SUBMITTED\""),
        (TaskState::Working,        "\"TASK_STATE_WORKING\""),
        (TaskState::Completed,      "\"TASK_STATE_COMPLETED\""),
        (TaskState::Failed,         "\"TASK_STATE_FAILED\""),
        (TaskState::Canceled,       "\"TASK_STATE_CANCELED\""),
        (TaskState::InputRequired,  "\"TASK_STATE_INPUT_REQUIRED\""),
        (TaskState::AuthRequired,   "\"TASK_STATE_AUTH_REQUIRED\""),
        (TaskState::Rejected,       "\"TASK_STATE_REJECTED\""),
    ];
    for (state, expected) in cases {
        assert_eq!(serde_json::to_string(&state).unwrap(), expected, "{state:?}");
    }
}
