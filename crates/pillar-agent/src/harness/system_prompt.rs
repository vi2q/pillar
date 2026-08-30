//! Port of packages/agent/src/harness/system-prompt.ts (pi v0.84.3).
//!
//! Formats the model-visible skills block for the system prompt.

use super::types::Skill;

/// Format the spec-compatible (agentskills.io) XML skills block. Skills
/// with `disable_model_invocation` are hidden from the model. Returns an
/// empty string when no visible skills remain.
pub fn format_skills_for_system_prompt(skills: &[Skill]) -> String {
    let visible_skills: Vec<&Skill> = skills
        .iter()
        .filter(|skill| !skill.disable_model_invocation)
        .collect();
    if visible_skills.is_empty() {
        return String::new();
    }

    let mut lines = vec![
        "The following skills provide specialized instructions for specific tasks.".to_owned(),
        "Read the full skill file when the task matches its description.".to_owned(),
        "When a skill file references a relative path, resolve it against the skill directory (parent of SKILL.md / dirname of the path) and use that absolute path in tool commands.".to_owned(),
        String::new(),
        "<available_skills>".to_owned(),
    ];

    for skill in visible_skills {
        lines.push("  <skill>".to_owned());
        lines.push(format!("    <name>{}</name>", escape_xml(&skill.name)));
        lines.push(format!(
            "    <description>{}</description>",
            escape_xml(&skill.description)
        ));
        lines.push(format!(
            "    <location>{}</location>",
            escape_xml(&skill.file_path)
        ));
        lines.push("  </skill>".to_owned());
    }

    lines.push("</available_skills>".to_owned());
    lines.join("\n")
}

fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill(name: &str, description: &str, path: &str, disabled: bool) -> Skill {
        Skill {
            name: name.to_owned(),
            description: description.to_owned(),
            content: "content".to_owned(),
            file_path: path.to_owned(),
            disable_model_invocation: disabled,
        }
    }

    #[test]
    fn formats_visible_skills_in_order_and_skips_disabled() {
        let skills = vec![
            skill(
                "visible",
                "Use <this> & that",
                "/skills/visible/SKILL.md",
                false,
            ),
            skill("hidden", "Hidden", "/skills/hidden/SKILL.md", true),
            skill("second", "Second skill", "/skills/second/SKILL.md", false),
        ];
        let expected = "The following skills provide specialized instructions for specific tasks.\nRead the full skill file when the task matches its description.\nWhen a skill file references a relative path, resolve it against the skill directory (parent of SKILL.md / dirname of the path) and use that absolute path in tool commands.\n\n<available_skills>\n  <skill>\n    <name>visible</name>\n    <description>Use &lt;this&gt; &amp; that</description>\n    <location>/skills/visible/SKILL.md</location>\n  </skill>\n  <skill>\n    <name>second</name>\n    <description>Second skill</description>\n    <location>/skills/second/SKILL.md</location>\n  </skill>\n</available_skills>";
        assert_eq!(format_skills_for_system_prompt(&skills), expected);
    }

    #[test]
    fn returns_empty_when_no_skills_visible() {
        let skills = vec![skill("hidden", "Hidden", "/skills/hidden/SKILL.md", true)];
        assert_eq!(format_skills_for_system_prompt(&skills), "");
    }

    #[test]
    fn escapes_xml_in_all_fields() {
        let skills = vec![skill(
            "a&b",
            "Quote \"double\" and 'single'",
            "/skills/<bad>&\"quote\"/SKILL.md",
            false,
        )];
        let output = format_skills_for_system_prompt(&skills);
        assert!(output.contains(
            "<name>a&amp;b</name>\n    <description>Quote &quot;double&quot; and &apos;single&apos;</description>\n    <location>/skills/&lt;bad&gt;&amp;&quot;quote&quot;/SKILL.md</location>"
        ));
    }
}
