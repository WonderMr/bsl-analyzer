use crate::define_metadata;
use crate::metadata::*;
use crate::AnalysisContext;
use crate::{BodyContext, Diagnostic, DiagnosticCode};
use hir::dataflow::reaching_defs::{DefSite, ReachingDefsResult};
use hir::str_template::{
    constant_template_texts, count_required_params, is_str_template_name, is_wrong_str_template,
    template_texts,
};
use hir::LocalRange;
use hir::{Expr, ExprId, ExprIdx, IdConversion, Name, Stmt, StmtId};
use stdx::case::contains_ignore_case;

pub const METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: DiagnosticType::Error,
    severity: DiagnosticSeverityLevel::Blocker,
    scope: DiagnosticScope::All,
    modules: &[],
    minutes_to_fix: 1,
    activated_by_default: true,
    compatibility_mode: DiagnosticCompatibilityMode::Undefined,
    tags: &[MetadataTag::Brainoverload, MetadataTag::Suspicious, MetadataTag::Unpredictable],
    can_locate_on_project: false,
    extra_min_for_complexity: 0.0,
    lsp_severity_override: "",
};

fn has_str_template_calls(text: &str) -> bool {
    contains_ignore_case(text, "стршаблон") || contains_ignore_case(text, "strtemplate")
}

pub fn check_body(ctx: &BodyContext, acc: &mut Vec<Diagnostic<LocalRange>>) {
    let code = DiagnosticCode::IncorrectUseOfStrTemplate;

    if ctx.is_disabled_with_metadata(code) || ctx.is_module_code() {
        return;
    }

    if !has_str_template_calls(&ctx.root().text().to_string()) {
        return;
    }

    let body = ctx.body();
    let source_map = ctx.source_map();
    let mut candidates: Vec<(StmtId, ExprIdx, usize)> = Vec::new();

    for (stmt_id, stmt) in body.stmts_iter() {
        let expr_id = match stmt {
            Stmt::Expr(id) => Some(*id),
            Stmt::Assign { value, .. } => Some(*value),
            _ => None,
        };

        if let Some(expr_id) = expr_id {
            let expr = body.expr(ExprId::from_idx(expr_id));

            let (method_name, args) = match expr {
                Expr::Call { callee, args } => {
                    if let Expr::Path(name) = body.expr(ExprId::from_idx(*callee)) {
                        (name.as_str(), args)
                    } else {
                        continue;
                    }
                }
                Expr::MethodCall { method, args, .. } => (method.as_str(), args),
                _ => continue,
            };

            if !is_str_template_name(method_name) {
                continue;
            }

            if args.is_empty() {
                continue;
            }

            let template = args[0];
            let param_count = args.len() - 1;

            // A template whose texts follow from the call alone is checked in lowering.
            if constant_template_texts(body, template).is_some() {
                continue;
            }

            candidates.push((stmt_id, template, param_count));
        }
    }

    if candidates.is_empty() {
        return;
    }

    let Some(reaching_defs) = ctx.reaching_definitions() else { return };

    for (stmt_id, template, param_count) in candidates {
        let texts = resolve_template_texts(template, body, &reaching_defs, stmt_id, 0);
        let wrong = texts.iter().flatten().find(|text| is_wrong_str_template(text, param_count));
        if let Some(template_string) = wrong {
            if let Some(range) = source_map.expr_range(ExprId::from_idx(template)) {
                acc.push(Diagnostic {
                    code,
                    message: format!(
                        "Template '{}' requires {} parameters but {} provided",
                        template_string.chars().take(50).collect::<String>(),
                        count_required_params(template_string),
                        param_count
                    ),
                    severity: ctx.severity(code),
                    range,
                    tags: ctx.tags(code),
                    fixes: vec![],
                });
            }
        }
    }
}

/// The texts of the template `expr` of the statement `stmt_id`, each variable in it read
/// through the definitions that reach the statement.
fn resolve_template_texts(
    expr: ExprIdx,
    body: &hir::Body,
    reaching_defs: &ReachingDefsResult,
    stmt_id: StmtId,
    depth: u32,
) -> Option<Vec<String>> {
    const MAX_DEPTH: u32 = 10;

    if depth > MAX_DEPTH {
        return None;
    }

    template_texts(body, expr, &mut |name: &Name| {
        resolve_variable_texts(name, body, reaching_defs, stmt_id, depth + 1)
    })
}

/// The texts of the variable `name` at `stmt_id`: known when every definition reaching the
/// statement is an assignment and all of them give the same texts. A parameter, a loop
/// variable or a second value leaves the variable unknown.
fn resolve_variable_texts(
    name: &Name,
    body: &hir::Body,
    reaching_defs: &ReachingDefsResult,
    stmt_id: StmtId,
    depth: u32,
) -> Option<Vec<String>> {
    let mut texts: Option<Vec<String>> = None;
    for def in reaching_defs.defs_for_var_at_stmt(name.as_str(), stmt_id)? {
        let DefSite::Assignment(assign_raw_idx) = def.def_site else {
            return None;
        };
        let assign_stmt_id = StmtId::from_raw(assign_raw_idx);
        let Stmt::Assign { value, .. } = body.stmt(assign_stmt_id) else {
            return None;
        };
        let def_texts = resolve_template_texts(*value, body, reaching_defs, assign_stmt_id, depth)?;
        if texts.as_ref().is_some_and(|known| *known != def_texts) {
            return None;
        }
        texts = Some(def_texts);
    }
    texts
}

pub fn from_hir(range: LocalRange, ctx: &AnalysisContext) -> Option<Diagnostic<LocalRange>> {
    let code = DiagnosticCode::IncorrectUseOfStrTemplate;

    if ctx.is_disabled_with_metadata(code) {
        return None;
    }

    Some(Diagnostic {
        code,
        message: "Некорректное использование СтрШаблон".to_string(),
        severity: ctx.severity(code),
        range,
        tags: ctx.tags(code),
        fixes: vec![],
    })
}

#[cfg(test)]
mod tests {
    use crate::test_utils::{check_diagnostics_snapshot_for, check_hir_diagnostic, format_diags};
    use crate::DiagnosticCode;
    use expect_test::expect;

    #[test]
    fn test_correct_usage() {
        let code = r#"
Процедура Тест()
    Г = СтрШаблон("Наименование (версия %1)", Версия());
КонецПроцедуры
"#;
        let diagnostics = check_hir_diagnostic(code);
        let filtered: Vec<_> = diagnostics
            .into_iter()
            .filter(|d| d.code == DiagnosticCode::IncorrectUseOfStrTemplate)
            .collect();
        expect![[r#""#]].assert_eq(&format_diags(code, &filtered));
    }

    #[test]
    fn test_missing_parameter() {
        let code = r#"
Процедура Тест()
    А = СтрШаблон("Наименование (версия %1)");
КонецПроцедуры
"#;
        let diagnostics = check_hir_diagnostic(code);
        let filtered: Vec<_> = diagnostics
            .into_iter()
            .filter(|d| d.code == DiagnosticCode::IncorrectUseOfStrTemplate)
            .collect();
        expect![[r#"
            IncorrectUseOfStrTemplate @ 3:9..3:46
              message: Некорректное использование СтрШаблон
              severity: Blocker"#]]
        .assert_eq(&format_diags(code, &filtered));
    }

    #[test]
    fn test_insufficient_arguments() {
        let code = r#"
Процедура Тест()
    Б = СтрШаблон("%1 (версия %2)", Наименование);
КонецПроцедуры
"#;
        let diagnostics = check_hir_diagnostic(code);
        let filtered: Vec<_> = diagnostics
            .into_iter()
            .filter(|d| d.code == DiagnosticCode::IncorrectUseOfStrTemplate)
            .collect();
        expect![[r#"
            IncorrectUseOfStrTemplate @ 3:9..3:50
              message: Некорректное использование СтрШаблон
              severity: Blocker"#]]
        .assert_eq(&format_diags(code, &filtered));
    }

    #[test]
    fn test_comprehensive() {
        use ide_db::{
            base_db::{SourceDatabase, SourceRoot, SourceRootId},
            RootDatabase, RootDatabaseImpl,
        };
        use std::sync::Arc;
        use test_fixture::Fixture;

        let code = r#"Процедура Метод()

    А = СтрШаблон("Наименование (версия %1)"); // ошибка

    Б = СтрШаблон("%1 (версия %2)", Наименование); // ошибка

    К = СтрШаблон("Наименование %11", Наименование); // ошибка

    К = СтрШаблон("Наименование %0", Наименование); // ошибка

    Ж = СтрШаблон("Наименование %2 (версия %%3)", Наименование, Версия); // ошибка

    //здесь ошибочно не закрыта скобка для НСтр
    В = СтрШаблон(НСтр("ru='Наименование (версия %1)'", Версия())); // ошибка

    НовыйШаблон = "123";
    Н = СтрШаблон(НовыйШаблон, Наименование); // ошибка

    НовыйШаблон1 = "123";
    ДругаяСтрока = "5487";
    Н = СтрШаблон(НовыйШаблон1, Наименование); // ошибка

    //НовыйШаблон2 = НСтр("ru='Наименование (версия)'";
    НовыйШаблон2 = "5487";
    Н = СтрШаблон(НовыйШаблон2, Наименование); // ошибка

    // ошибка
    С24 = СтрШаблон("%1, %2, %3, %4, %5, %6, %7, %8, %9, %10, %11", "ф", "ф", "ф", "ф", "ф", "ф", "ф", "ф", "Ф", "");

    Л = СтрШаблон("Наименование %(1)"); // ошибка

    Г = СтрШаблон(НСтр("ru='Наименование (версия %1)'"), Версия());

    Д = СтрШаблон("Наименование (версия)");

    Е = СтрШаблон("Наименование (версия %1)", Наименование);

    Е = СтрШаблон("Наименование %1 (версия %2)", Наименование, Версия);

    З = СтрШаблон("Наименование %%1 (версия %%2)");
    Ий = СтрШаблон("Наименование %1 (версия %%2)", Наименование);

    Л = СтрШаблон("Наименование %(1)1", Наименование); // в СП разрешен такой вариант

    М = СтрШаблон(ШаблонНаименования, Наименование);
    М = СтрШаблон("123" + ШаблонНаименования, Наименование);

    НовыйШаблон3 = "%1";
    Н = СтрШаблон(НовыйШаблон3, Наименование);

    А = СтрШаблон("%(1)%(2)", "Первая", 2);

    Б = СтрШаблон("%%%1%%", "Первая");

    Объект.НовыйШаблон4 = "%1"; // новый код
    Н = СтрШаблон(Объект.НовыйШаблон4, Наименование);

    Объект.НовыйШаблон5Ошибка = "%1 %2"; // падает на этой строчке, что верно
    Н = СтрШаблон(Объект.НовыйШаблон5Ошибка, Наименование);
КонецПроцедуры
"#;
        let fixture_text = format!("//- /test.bsl\n{}", code);
        let fixture = Fixture::parse(&fixture_text);
        let file_id = fixture.first_file().expect("fixture should have a file");

        let mut db = RootDatabaseImpl::new();

        let mut file_set = vfs::FileSet::default();
        file_set.insert(file_id, vfs::VfsPath::new("/test.bsl"));
        let source_root = SourceRoot::new_local(file_set);
        db.set_source_root(SourceRootId(0), source_root);
        db.set_file_source_root(file_id, SourceRootId(0));

        for (fid, file) in &fixture.files {
            db.set_file_text(*fid, &file.content);
        }

        #[allow(clippy::arc_with_non_send_sync)]
        let db = Arc::new(db) as Arc<dyn RootDatabase>;

        let config = crate::DiagnosticsConfig::default();
        let provider = ide_db::SalsaProvider::new(db.as_ref(), None);
        let ctx = crate::DiagnosticsContext::new(&config, file_id, &provider);

        let diagnostics = crate::diagnostics(&ctx);

        let filtered: Vec<_> = diagnostics
            .into_iter()
            .filter(|d| d.code == DiagnosticCode::IncorrectUseOfStrTemplate)
            .collect();

        expect![[r#"
            IncorrectUseOfStrTemplate @ 3:9..3:46
              message: Некорректное использование СтрШаблон
              severity: Blocker
            IncorrectUseOfStrTemplate @ 5:9..5:50
              message: Некорректное использование СтрШаблон
              severity: Blocker
            IncorrectUseOfStrTemplate @ 7:9..7:52
              message: Некорректное использование СтрШаблон
              severity: Blocker
            IncorrectUseOfStrTemplate @ 9:9..9:51
              message: Некорректное использование СтрШаблон
              severity: Blocker
            IncorrectUseOfStrTemplate @ 11:9..11:72
              message: Некорректное использование СтрШаблон
              severity: Blocker
            IncorrectUseOfStrTemplate @ 14:9..14:67
              message: Некорректное использование СтрШаблон
              severity: Blocker
            IncorrectUseOfStrTemplate @ 17:19..17:30
              message: Template '123' requires 0 parameters but 1 provided
              severity: Blocker
            IncorrectUseOfStrTemplate @ 21:19..21:31
              message: Template '123' requires 0 parameters but 1 provided
              severity: Blocker
            IncorrectUseOfStrTemplate @ 25:19..25:31
              message: Template '5487' requires 0 parameters but 1 provided
              severity: Blocker
            IncorrectUseOfStrTemplate @ 28:11..28:117
              message: Некорректное использование СтрШаблон
              severity: Blocker
            IncorrectUseOfStrTemplate @ 30:9..30:39
              message: Некорректное использование СтрШаблон
              severity: Blocker"#]]
        .assert_eq(&format_diags(code, &filtered));
    }

    #[test]
    fn template_added_up_from_literals_is_checked_as_one_string() {
        check_diagnostics_snapshot_for(
            include_str!("fixtures/str_template_literal_concatenation.bsl"),
            DiagnosticCode::IncorrectUseOfStrTemplate,
            expect![[r#"
                IncorrectUseOfStrTemplate @ 31:13..31:56
                  message: Некорректное использование СтрШаблон
                  severity: Blocker
                IncorrectUseOfStrTemplate @ 32:13..32:73
                  message: Некорректное использование СтрШаблон
                  severity: Blocker
                IncorrectUseOfStrTemplate @ 33:13..33:69
                  message: Некорректное использование СтрШаблон
                  severity: Blocker
                IncorrectUseOfStrTemplate @ 34:13..34:67
                  message: Некорректное использование СтрШаблон
                  severity: Blocker
                IncorrectUseOfStrTemplate @ 35:13..36:24
                  message: Некорректное использование СтрШаблон
                  severity: Blocker
                IncorrectUseOfStrTemplate @ 37:13..37:57
                  message: Некорректное использование СтрШаблон
                  severity: Blocker"#]],
        );
    }

    #[test]
    fn chars_parts_of_a_template_are_text_without_markers() {
        check_diagnostics_snapshot_for(
            include_str!("fixtures/str_template_chars_parts.bsl"),
            DiagnosticCode::IncorrectUseOfStrTemplate,
            expect![[r#"
                IncorrectUseOfStrTemplate @ 11:13..11:63
                  message: Некорректное использование СтрШаблон
                  severity: Blocker
                IncorrectUseOfStrTemplate @ 12:13..12:55
                  message: Некорректное использование СтрШаблон
                  severity: Blocker
                IncorrectUseOfStrTemplate @ 13:13..13:79
                  message: Некорректное использование СтрШаблон
                  severity: Blocker"#]],
        );
    }

    #[test]
    fn each_constant_branch_of_a_conditional_template_is_checked() {
        check_diagnostics_snapshot_for(
            include_str!("fixtures/str_template_conditional_branches.bsl"),
            DiagnosticCode::IncorrectUseOfStrTemplate,
            expect![[r#"
                IncorrectUseOfStrTemplate @ 11:13..11:78
                  message: Некорректное использование СтрШаблон
                  severity: Blocker
                IncorrectUseOfStrTemplate @ 12:13..12:69
                  message: Некорректное использование СтрШаблон
                  severity: Blocker
                IncorrectUseOfStrTemplate @ 13:13..13:72
                  message: Некорректное использование СтрШаблон
                  severity: Blocker"#]],
        );
    }

    #[test]
    fn each_translation_of_a_multilingual_template_is_checked() {
        check_diagnostics_snapshot_for(
            include_str!("fixtures/str_template_nstr_languages.bsl"),
            DiagnosticCode::IncorrectUseOfStrTemplate,
            expect![[r#"
                IncorrectUseOfStrTemplate @ 11:13..11:89
                  message: Некорректное использование СтрШаблон
                  severity: Blocker
                IncorrectUseOfStrTemplate @ 12:13..12:74
                  message: Некорректное использование СтрШаблон
                  severity: Blocker
                IncorrectUseOfStrTemplate @ 13:13..13:94
                  message: Некорректное использование СтрШаблон
                  severity: Blocker
                IncorrectUseOfStrTemplate @ 14:13..14:93
                  message: Некорректное использование СтрШаблон
                  severity: Blocker"#]],
        );
    }

    #[test]
    fn template_variable_assigned_from_parts_is_checked() {
        check_diagnostics_snapshot_for(
            include_str!("fixtures/str_template_through_variable.bsl"),
            DiagnosticCode::IncorrectUseOfStrTemplate,
            expect![[r#"
                IncorrectUseOfStrTemplate @ 22:23..22:29
                  message: Template 'Документ %1 от %2' requires 2 parameters but 1 provided
                  severity: Blocker
                IncorrectUseOfStrTemplate @ 24:23..24:36
                  message: Template 'Документ %1 без даты' requires 1 parameters but 2 provided
                  severity: Blocker
                IncorrectUseOfStrTemplate @ 27:23..27:43
                  message: Template 'Документ %1 от %2.' requires 2 parameters but 1 provided
                  severity: Blocker
                IncorrectUseOfStrTemplate @ 29:23..29:37
                  message: Template 'Document' requires 0 parameters but 1 provided
                  severity: Blocker"#]],
        );
    }

    #[test]
    fn test_variable_resolution_simple() {
        use ide_db::{
            base_db::{SourceDatabase, SourceRoot, SourceRootId},
            RootDatabase, RootDatabaseImpl,
        };
        use std::sync::Arc;
        use test_fixture::Fixture;

        let code = r#"
Процедура Тест()
    НовыйШаблон = "123";
    А = СтрШаблон(НовыйШаблон, Наименование); // ошибка: "123" не содержит %1
КонецПроцедуры
"#;
        let fixture_text = format!("//- /test.bsl\n{}", code);
        let fixture = Fixture::parse(&fixture_text);
        let file_id = fixture.first_file().expect("fixture should have a file");

        let mut db = RootDatabaseImpl::new();

        let mut file_set = vfs::FileSet::default();
        file_set.insert(file_id, vfs::VfsPath::new("/test.bsl"));
        let source_root = SourceRoot::new_local(file_set);
        db.set_source_root(SourceRootId(0), source_root);
        db.set_file_source_root(file_id, SourceRootId(0));

        for (fid, file) in &fixture.files {
            db.set_file_text(*fid, &file.content);
        }

        #[allow(clippy::arc_with_non_send_sync)]
        let db = Arc::new(db) as Arc<dyn RootDatabase>;

        let config = crate::DiagnosticsConfig::default();
        let provider = ide_db::SalsaProvider::new(db.as_ref(), None);
        let ctx = crate::DiagnosticsContext::new(&config, file_id, &provider);

        let diagnostics = crate::diagnostics(&ctx);
        let filtered: Vec<_> = diagnostics
            .into_iter()
            .filter(|d| d.code == DiagnosticCode::IncorrectUseOfStrTemplate)
            .collect();
        expect![[r#"
            IncorrectUseOfStrTemplate @ 4:19..4:30
              message: Template '123' requires 0 parameters but 1 provided
              severity: Blocker"#]]
        .assert_eq(&format_diags(code, &filtered));
    }

    #[test]
    fn test_variable_resolution_with_template() {
        use ide_db::{
            base_db::{SourceDatabase, SourceRoot, SourceRootId},
            RootDatabase, RootDatabaseImpl,
        };
        use std::sync::Arc;
        use test_fixture::Fixture;

        let code = r#"
Процедура Тест()
    НовыйШаблон = "%1";
    А = СтрШаблон(НовыйШаблон, Наименование); // OK
КонецПроцедуры
"#;
        let fixture_text = format!("//- /test.bsl\n{}", code);
        let fixture = Fixture::parse(&fixture_text);
        let file_id = fixture.first_file().expect("fixture should have a file");

        let mut db = RootDatabaseImpl::new();

        let mut file_set = vfs::FileSet::default();
        file_set.insert(file_id, vfs::VfsPath::new("/test.bsl"));
        let source_root = SourceRoot::new_local(file_set);
        db.set_source_root(SourceRootId(0), source_root);
        db.set_file_source_root(file_id, SourceRootId(0));

        for (fid, file) in &fixture.files {
            db.set_file_text(*fid, &file.content);
        }

        #[allow(clippy::arc_with_non_send_sync)]
        let db = Arc::new(db) as Arc<dyn RootDatabase>;

        let config = crate::DiagnosticsConfig::default();
        let provider = ide_db::SalsaProvider::new(db.as_ref(), None);
        let ctx = crate::DiagnosticsContext::new(&config, file_id, &provider);

        let diagnostics = crate::diagnostics(&ctx);
        let filtered: Vec<_> = diagnostics
            .into_iter()
            .filter(|d| d.code == DiagnosticCode::IncorrectUseOfStrTemplate)
            .collect();
        expect![[r#""#]].assert_eq(&format_diags(code, &filtered));
    }

    #[test]
    fn test_variable_resolution_conditional() {
        use ide_db::{
            base_db::{SourceDatabase, SourceRoot, SourceRootId},
            RootDatabase, RootDatabaseImpl,
        };
        use std::sync::Arc;
        use test_fixture::Fixture;

        let code = r#"
Процедура Тест()
    Если Условие Тогда
        Шаблон = "123";
    Иначе
        Шаблон = "%1";
    КонецЕсли;
    А = СтрШаблон(Шаблон, Наименование);
КонецПроцедуры
"#;
        let fixture_text = format!("//- /test.bsl\n{}", code);
        let fixture = Fixture::parse(&fixture_text);
        let file_id = fixture.first_file().expect("fixture should have a file");

        let mut db = RootDatabaseImpl::new();

        let mut file_set = vfs::FileSet::default();
        file_set.insert(file_id, vfs::VfsPath::new("/test.bsl"));
        let source_root = SourceRoot::new_local(file_set);
        db.set_source_root(SourceRootId(0), source_root);
        db.set_file_source_root(file_id, SourceRootId(0));

        for (fid, file) in &fixture.files {
            db.set_file_text(*fid, &file.content);
        }

        #[allow(clippy::arc_with_non_send_sync)]
        let db = Arc::new(db) as Arc<dyn RootDatabase>;

        let config = crate::DiagnosticsConfig::default();
        let provider = ide_db::SalsaProvider::new(db.as_ref(), None);
        let ctx = crate::DiagnosticsContext::new(&config, file_id, &provider);

        let diagnostics = crate::diagnostics(&ctx);
        let filtered: Vec<_> = diagnostics
            .into_iter()
            .filter(|d| d.code == DiagnosticCode::IncorrectUseOfStrTemplate)
            .collect();
        expect![[r#""#]].assert_eq(&format_diags(code, &filtered));
    }

    #[test]
    fn test_transitive_assignment() {
        use ide_db::{
            base_db::{SourceDatabase, SourceRoot, SourceRootId},
            RootDatabase, RootDatabaseImpl,
        };
        use std::sync::Arc;
        use test_fixture::Fixture;

        let code = r#"
Процедура Тест()
    Шаблон1 = "template %1";
    Шаблон2 = Шаблон1;
    А = СтрШаблон(Шаблон2, Наименование); // OK - resolves through transitive assignment
КонецПроцедуры
"#;
        let fixture_text = format!("//- /test.bsl\n{}", code);
        let fixture = Fixture::parse(&fixture_text);
        let file_id = fixture.first_file().expect("fixture should have a file");

        let mut db = RootDatabaseImpl::new();

        let mut file_set = vfs::FileSet::default();
        file_set.insert(file_id, vfs::VfsPath::new("/test.bsl"));
        let source_root = SourceRoot::new_local(file_set);
        db.set_source_root(SourceRootId(0), source_root);
        db.set_file_source_root(file_id, SourceRootId(0));

        for (fid, file) in &fixture.files {
            db.set_file_text(*fid, &file.content);
        }

        #[allow(clippy::arc_with_non_send_sync)]
        let db = Arc::new(db) as Arc<dyn RootDatabase>;

        let config = crate::DiagnosticsConfig::default();
        let provider = ide_db::SalsaProvider::new(db.as_ref(), None);
        let ctx = crate::DiagnosticsContext::new(&config, file_id, &provider);

        let diagnostics = crate::diagnostics(&ctx);
        let filtered: Vec<_> = diagnostics
            .into_iter()
            .filter(|d| d.code == DiagnosticCode::IncorrectUseOfStrTemplate)
            .collect();
        expect![[r#""#]].assert_eq(&format_diags(code, &filtered));
    }

    #[test]
    fn test_transitive_assignment_error() {
        use ide_db::{
            base_db::{SourceDatabase, SourceRoot, SourceRootId},
            RootDatabase, RootDatabaseImpl,
        };
        use std::sync::Arc;
        use test_fixture::Fixture;

        let code = r#"
Процедура Тест()
    Шаблон1 = "no placeholders";
    Шаблон2 = Шаблон1;
    А = СтрШаблон(Шаблон2, Наименование); // Error - resolves to "no placeholders"
КонецПроцедуры
"#;
        let fixture_text = format!("//- /test.bsl\n{}", code);
        let fixture = Fixture::parse(&fixture_text);
        let file_id = fixture.first_file().expect("fixture should have a file");

        let mut db = RootDatabaseImpl::new();

        let mut file_set = vfs::FileSet::default();
        file_set.insert(file_id, vfs::VfsPath::new("/test.bsl"));
        let source_root = SourceRoot::new_local(file_set);
        db.set_source_root(SourceRootId(0), source_root);
        db.set_file_source_root(file_id, SourceRootId(0));

        for (fid, file) in &fixture.files {
            db.set_file_text(*fid, &file.content);
        }

        #[allow(clippy::arc_with_non_send_sync)]
        let db = Arc::new(db) as Arc<dyn RootDatabase>;

        let config = crate::DiagnosticsConfig::default();
        let provider = ide_db::SalsaProvider::new(db.as_ref(), None);
        let ctx = crate::DiagnosticsContext::new(&config, file_id, &provider);

        let diagnostics = crate::diagnostics(&ctx);
        let filtered: Vec<_> = diagnostics
            .into_iter()
            .filter(|d| d.code == DiagnosticCode::IncorrectUseOfStrTemplate)
            .collect();
        expect![[r#"
            IncorrectUseOfStrTemplate @ 5:19..5:26
              message: Template 'no placeholders' requires 0 parameters but 1 provided
              severity: Blocker"#]]
        .assert_eq(&format_diags(code, &filtered));
    }

    #[test]
    fn test_deep_transitive_chain() {
        use ide_db::{
            base_db::{SourceDatabase, SourceRoot, SourceRootId},
            RootDatabase, RootDatabaseImpl,
        };
        use std::sync::Arc;
        use test_fixture::Fixture;

        let code = r#"
Процедура Тест()
    Ш1 = "template %1";
    Ш2 = Ш1;
    Ш3 = Ш2;
    Ш4 = Ш3;
    А = СтрШаблон(Ш4, Наименование); // OK - resolves through chain
КонецПроцедуры
"#;
        let fixture_text = format!("//- /test.bsl\n{}", code);
        let fixture = Fixture::parse(&fixture_text);
        let file_id = fixture.first_file().expect("fixture should have a file");

        let mut db = RootDatabaseImpl::new();

        let mut file_set = vfs::FileSet::default();
        file_set.insert(file_id, vfs::VfsPath::new("/test.bsl"));
        let source_root = SourceRoot::new_local(file_set);
        db.set_source_root(SourceRootId(0), source_root);
        db.set_file_source_root(file_id, SourceRootId(0));

        for (fid, file) in &fixture.files {
            db.set_file_text(*fid, &file.content);
        }

        #[allow(clippy::arc_with_non_send_sync)]
        let db = Arc::new(db) as Arc<dyn RootDatabase>;

        let config = crate::DiagnosticsConfig::default();
        let provider = ide_db::SalsaProvider::new(db.as_ref(), None);
        let ctx = crate::DiagnosticsContext::new(&config, file_id, &provider);

        let diagnostics = crate::diagnostics(&ctx);
        let filtered: Vec<_> = diagnostics
            .into_iter()
            .filter(|d| d.code == DiagnosticCode::IncorrectUseOfStrTemplate)
            .collect();
        expect![[r#""#]].assert_eq(&format_diags(code, &filtered));
    }

    #[test]
    fn test_multiple_defs_same_value() {
        use ide_db::{
            base_db::{SourceDatabase, SourceRoot, SourceRootId},
            RootDatabase, RootDatabaseImpl,
        };
        use std::sync::Arc;
        use test_fixture::Fixture;
        let code = r#"
Процедура Тест()
    Если Условие Тогда
        Шаблон = "template %1";
    Иначе
        Шаблон = "template %1";  // Same value as then branch
    КонецЕсли;
    А = СтрШаблон(Шаблон, Наименование); // OK - both branches give same value
КонецПроцедуры
"#;
        let fixture_text = format!("//- /test.bsl\n{}", code);
        let fixture = Fixture::parse(&fixture_text);
        let file_id = fixture.first_file().expect("fixture should have a file");

        let mut db = RootDatabaseImpl::new();

        let mut file_set = vfs::FileSet::default();
        file_set.insert(file_id, vfs::VfsPath::new("/test.bsl"));
        let source_root = SourceRoot::new_local(file_set);
        db.set_source_root(SourceRootId(0), source_root);
        db.set_file_source_root(file_id, SourceRootId(0));

        for (fid, file) in &fixture.files {
            db.set_file_text(*fid, &file.content);
        }

        #[allow(clippy::arc_with_non_send_sync)]
        let db = Arc::new(db) as Arc<dyn RootDatabase>;

        let config = crate::DiagnosticsConfig::default();
        let provider = ide_db::SalsaProvider::new(db.as_ref(), None);
        let ctx = crate::DiagnosticsContext::new(&config, file_id, &provider);

        let diagnostics = crate::diagnostics(&ctx);
        let filtered: Vec<_> = diagnostics
            .into_iter()
            .filter(|d| d.code == DiagnosticCode::IncorrectUseOfStrTemplate)
            .collect();
        expect![[r#""#]].assert_eq(&format_diags(code, &filtered));
    }
}
