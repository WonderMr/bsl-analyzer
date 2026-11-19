use bsl_metadata::metadata_object::MdoType;
use bsl_metadata::traits::MdObject;
use bsl_metadata::Configuration;
use rmcp::model::{CallToolResult, ContentBlock};
use rmcp::ErrorData as McpError;
use std::collections::BTreeMap;
use std::fmt::Write;

pub async fn get_live_metadata_tree(
    state: &crate::SharedState,
    connection: Option<&str>,
    meta_type: &str,
    name_mask: Option<String>,
    limit: u32,
    max_output_tokens: usize,
) -> Result<CallToolResult, McpError> {
    let selected =
        state.onec_connection(connection).map_err(|e| McpError::invalid_params(e, None))?;
    let client = selected
        .http_client()
        .ok_or_else(|| McpError::invalid_params("This 1C connection has no HTTP backend", None))?;
    let result = client
        .list_metadata(&onec_client::MetadataListRequest {
            meta_type: meta_type.to_string(),
            name_mask,
            limit: limit.clamp(1, 1000),
        })
        .await
        .map_err(live_metadata_error)?;
    live_metadata_tree_response(connection, result, max_output_tokens)
}

pub async fn get_live_metadata_object(
    state: &crate::SharedState,
    connection: Option<&str>,
    meta_type: &str,
    name: &str,
    max_output_tokens: usize,
) -> Result<CallToolResult, McpError> {
    let selected =
        state.onec_connection(connection).map_err(|e| McpError::invalid_params(e, None))?;
    let client = selected
        .http_client()
        .ok_or_else(|| McpError::invalid_params("This 1C connection has no HTTP backend", None))?;
    let value = client
        .metadata_structure(&onec_client::MetadataStructureRequest {
            meta_type: meta_type.to_string(),
            name: name.to_string(),
        })
        .await
        .map_err(live_metadata_error)?;
    live_metadata_object_response(connection, value, max_output_tokens)
}

fn live_metadata_error(error: onec_client::Error) -> McpError {
    McpError::internal_error(format!("Ошибка чтения метаданных 1С: {error}"), None)
}

// ponytail: each rejected candidate costs one full serialization, up to one pass per returned
// metadata item (O(n²) bytes in the largest case). Keep the simple exact check while a single
// metadata object stays comfortably small; if real producer objects make this slow, account for
// removed array suffixes incrementally and serialize only the final envelope.
fn live_metadata_object_response(
    connection: Option<&str>,
    object: onec_client::MetadataStructureResult,
    max_output_tokens: usize,
) -> Result<CallToolResult, McpError> {
    let mut object = serde_json::to_value(object).map_err(|error| {
        McpError::internal_error(format!("Ошибка сериализации метаданных 1С: {error}"), None)
    })?;
    let mut truncated = false;
    let ceiling = max_output_tokens.saturating_mul(4);

    loop {
        let completeness = live_object_completeness(&object, truncated);
        let result = crate::tools::response::structured(serde_json::json!({
            "schema_version": "2",
            "source": "infobase",
            "connection": connection,
            "object": object.clone(),
            "completeness": completeness,
            "truncated": truncated,
        }));
        if live_result_payload_bytes(&result) <= ceiling {
            return Ok(result);
        }

        let mut removed = false;
        for field in ["Ресурсы", "Измерения", "ТабличныеЧасти", "Реквизиты", "СтандартныеРеквизиты"]
        {
            if object
                .get_mut(field)
                .and_then(serde_json::Value::as_array_mut)
                .is_some_and(|items| items.pop().is_some())
            {
                removed = true;
                truncated = true;
                break;
            }
        }
        if !removed {
            return Err(metadata_budget_error(live_result_payload_bytes(&result)));
        }
    }
}

fn live_metadata_tree_response(
    connection: Option<&str>,
    result: onec_client::MetadataListResult,
    max_output_tokens: usize,
) -> Result<CallToolResult, McpError> {
    let mut items: Vec<_> = result
        .items
        .into_iter()
        .map(|item| {
            serde_json::json!({ "name": item.name, "full_name": item.full_name, "synonym": item.synonym })
        })
        .collect();
    let mut truncated = result.truncated;
    let ceiling = max_output_tokens.saturating_mul(4);

    loop {
        let response = crate::tools::response::structured(serde_json::json!({
            "schema_version": "2",
            "source": "infobase",
            "connection": connection,
            "items": items.clone(),
            "returned": items.len(),
            "truncated": truncated,
        }));
        if live_result_payload_bytes(&response) <= ceiling {
            return Ok(response);
        }
        if items.pop().is_none() {
            return Err(metadata_budget_error(live_result_payload_bytes(&response)));
        }
        truncated = true;
    }
}

fn live_result_payload_bytes(result: &CallToolResult) -> usize {
    crate::tools::response::serialized_bytes(&result.content)
        + result
            .structured_content
            .as_ref()
            .map(crate::tools::response::serialized_bytes)
            .unwrap_or_default()
}

fn metadata_budget_error(minimum_bytes: usize) -> McpError {
    McpError::invalid_params(
        "max_output_tokens is too small for the metadata response",
        Some(serde_json::json!({
            "reason": "budget_too_small",
            "minimum_output_tokens": minimum_bytes.saturating_add(3) / 4,
        })),
    )
}

fn live_object_completeness(object: &serde_json::Value, truncated: bool) -> serde_json::Value {
    let mut reasons = Vec::new();
    for field in ["СтандартныеРеквизиты", "Реквизиты", "Измерения", "Ресурсы"]
    {
        if let Some(items) = object.get(field).and_then(serde_json::Value::as_array) {
            for (index, item) in items.iter().enumerate() {
                let path = pointer_child(&pointer_child("", field), &index.to_string());
                push_unresolved_type_reasons(item, &path, &mut reasons);
            }
        }
    }
    if let Some(sections) = object.get("ТабличныеЧасти").and_then(serde_json::Value::as_array)
    {
        for (section_index, section) in sections.iter().enumerate() {
            let section_path =
                pointer_child(&pointer_child("", "ТабличныеЧасти"), &section_index.to_string());
            match section.get("attributes") {
                Some(serde_json::Value::Null) | None => reasons.push(serde_json::json!({
                    "code": "legacy_tabular_attributes",
                    "path": pointer_child(&section_path, "attributes"),
                })),
                Some(serde_json::Value::Array(attributes)) => {
                    for (attribute_index, attribute) in attributes.iter().enumerate() {
                        push_unresolved_type_reasons(
                            attribute,
                            &pointer_child(
                                &pointer_child(&section_path, "attributes"),
                                &attribute_index.to_string(),
                            ),
                            &mut reasons,
                        );
                    }
                }
                _ => unreachable!("tabular attributes are normalized to an array or null"),
            }
        }
    }
    if truncated {
        reasons.push(serde_json::json!({ "code": "output_budget", "path": "" }));
    }
    serde_json::json!({
        "status": if reasons.is_empty() { "complete" } else { "partial" },
        "reasons": reasons,
    })
}

fn pointer_child(parent: &str, segment: &str) -> String {
    let escaped = segment.replace('~', "~0").replace('/', "~1");
    format!("{parent}/{escaped}")
}

fn push_unresolved_type_reasons(
    item: &serde_json::Value,
    path: &str,
    reasons: &mut Vec<serde_json::Value>,
) {
    if let Some(variants) = item.get("type_variants").and_then(serde_json::Value::as_array) {
        for (index, variant) in variants.iter().enumerate() {
            if variant.get("resolution").and_then(serde_json::Value::as_str) == Some("unresolved") {
                reasons.push(serde_json::json!({
                    "code": "unresolved_type",
                    "path": pointer_child(
                        &pointer_child(&pointer_child(path, "type_variants"), &index.to_string()),
                        "technical_name",
                    ),
                }));
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServiceKind {
    Http,
    Web,
    Integration,
}

fn parse_service_kind(raw: &str) -> Option<ServiceKind> {
    match raw.to_lowercase().as_str() {
        "httpservice" | "httpservices" | "httpсервис" | "httpсервисы" => {
            Some(ServiceKind::Http)
        }
        "webservice" | "webservices" | "webсервис" | "webсервисы" => {
            Some(ServiceKind::Web)
        }
        "integrationservice" | "integrationservices" | "сервисинтеграции" | "сервисыинтеграции" => {
            Some(ServiceKind::Integration)
        }
        _ => None,
    }
}

fn dash(s: &str) -> &str {
    if s.is_empty() {
        "—"
    } else {
        s
    }
}

/// Walk a whole-configuration collection with a cancellation checkpoint per item.
///
/// These renderers only format what the substrate already holds: they run no salsa
/// query, so a cancelled request has no query boundary to unwind at and would list
/// every object in the configuration before anyone noticed. Every walk that scales with
/// configuration size goes through this iterator — the SELECTION of a category as much
/// as the listing of what it selected, because a category with no objects still costs a
/// full pass and never reaches the listing loop.
///
/// Where it deliberately does not go: rendering ONE entity — an object's attributes, a
/// register's dimensions, a service's operations. That work is bounded by the entity,
/// and the salsa query that resolved it is itself a checkpoint, so the only window left
/// is one object's worth of `writeln!`.
fn checkpointed<'a, I>(
    db: &'a ide::RootDatabaseImpl,
    items: I,
) -> impl Iterator<Item = I::Item> + 'a
where
    I: IntoIterator + 'a,
{
    items.into_iter().inspect(|_| salsa::Database::unwind_if_revision_cancelled(db))
}

/// The `tree` action: the category summary, one category's objects (`category`), or the
/// objects whose name contains `filter`, across the configuration and its extensions; with
/// both, the category's objects whose name contains `filter`.
pub fn get_metadata_tree(
    db: &ide::RootDatabaseImpl,
    config: &Configuration,
    extensions: &[(String, Configuration)],
    category: Option<String>,
    filter: Option<String>,
    max_output_tokens: usize,
) -> Result<CallToolResult, McpError> {
    let filter = filter.map(|text| text.trim().to_owned()).filter(|text| !text.is_empty());
    if let Some(ref category) = category {
        let filter = filter.as_deref();
        format_filtered_tree(db, config, extensions, category, filter, max_output_tokens)
    } else if let Some(ref filter) = filter {
        Ok(format_name_matches(db, config, extensions, filter, max_output_tokens))
    } else {
        let mut result = format_summary_tree(db, config)?;
        if !extensions.is_empty() {
            let text = result.content[0].as_text().expect("text").text.clone();
            let mut out = text;
            for (name, ext_config) in extensions {
                out.push_str(&format_extension_summary(db, name, ext_config));
            }
            result = CallToolResult::success(vec![ContentBlock::text(out)]);
        }
        Ok(result)
    }
}

/// `db` is here for one reason: these loops enumerate a whole configuration without
/// running a single salsa query, so a cancelled request is observed at the checkpoint
/// or not until the last object.
fn format_summary_tree(
    db: &ide::RootDatabaseImpl,
    config: &Configuration,
) -> Result<CallToolResult, McpError> {
    let mut categories: BTreeMap<&str, usize> = BTreeMap::new();

    for obj in checkpointed(db, config.metadata_objects()) {
        let key = obj.mdo_type.russian_name();
        *categories.entry(key).or_default() += 1;
    }

    for reg in checkpointed(db, config.registers()) {
        let key = reg.mdo_type().russian_name();
        *categories.entry(key).or_default() += 1;
    }

    let common_modules = config.common_modules().len();
    if common_modules > 0 {
        categories.insert("ОбщийМодуль", common_modules);
    }

    let event_subs = config.event_subscriptions().len();
    if event_subs > 0 {
        categories.insert("ПодпискаНаСобытие", event_subs);
    }

    let defined_types = config.defined_types().len();
    if defined_types > 0 {
        categories.insert("ОпределяемыйТип", defined_types);
    }

    let scheduled_jobs = config.scheduled_jobs().len();
    if scheduled_jobs > 0 {
        categories.insert("РегламентноеЗадание", scheduled_jobs);
    }

    let roles = config.roles().len();
    if roles > 0 {
        categories.insert("Роль", roles);
    }

    let http_services = config.http_services().len();
    if http_services > 0 {
        categories.insert("HTTPСервис", http_services);
    }

    let web_services = config.web_services().len();
    if web_services > 0 {
        categories.insert("WebСервис", web_services);
    }

    let integration_services = config.integration_services().len();
    if integration_services > 0 {
        categories.insert("СервисИнтеграции", integration_services);
    }

    let total: usize = categories.values().sum();
    let mut out = format!("# {} — дерево метаданных\n\n", config.name());
    let _ = writeln!(out, "Всего объектов: {total}\n");
    let _ = writeln!(out, "| Категория | Количество |");
    let _ = writeln!(out, "|-----------|------------|");
    for (category, count) in &categories {
        let _ = writeln!(out, "| {category} | {count} |");
    }
    let _ = writeln!(
        out,
        "\nИспользуйте `meta_type` для списка объектов категории и `filter` для поиска объектов \
         по части имени."
    );

    Ok(CallToolResult::success(vec![ContentBlock::text(out)]))
}

/// A category of the metadata tree: what `meta_type` names in source mode and what the summary
/// counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Category {
    /// The metadata objects and registers of one kind.
    Objects(MdoType),
    CommonModules,
    EventSubscriptions,
    DefinedTypes,
    ScheduledJobs,
    Roles,
    Services(ServiceKind),
}

impl Category {
    /// The category an agent names, in the singular or the plural, in Russian or English, or
    /// over-qualified with an object (`Справочник.Пользователи`). A category the configuration
    /// has no object of is still a category.
    fn parse(raw: &str) -> Option<Self> {
        let head = raw.split('.').next().unwrap_or(raw).trim().replace(['ё', 'Ё'], "е");
        let head = head.as_str();
        if let Some(category) = collection_category(&head.to_lowercase()) {
            return Some(category);
        }
        if let Some(kind) = parse_service_kind(head) {
            return Some(Self::Services(kind));
        }
        // English plurals are the dump's directory names (`Catalogs`); stripping the `s` gives
        // the singular for the kinds `from_plural` does not list.
        let mdo_type = head
            .parse::<MdoType>()
            .ok()
            .or_else(|| MdoType::from_plural(head))
            .or_else(|| head.strip_suffix('s').and_then(|singular| singular.parse().ok()))?;
        Some(Self::Objects(mdo_type))
    }

    /// The heading of the category's listing.
    fn heading(self) -> &'static str {
        match self {
            Self::Objects(mdo_type) => mdo_type.russian_name(),
            Self::CommonModules => "ОбщиеМодули",
            Self::EventSubscriptions => "ПодпискиНаСобытия",
            Self::DefinedTypes => "ОпределяемыеТипы",
            Self::ScheduledJobs => "РегламентныеЗадания",
            Self::Roles => "Роли",
            Self::Services(ServiceKind::Http) => "HTTPСервисы",
            Self::Services(ServiceKind::Web) => "WebСервисы",
            Self::Services(ServiceKind::Integration) => "СервисыИнтеграции",
        }
    }

    /// The category's objects in one configuration, in the order the configuration lists them.
    /// The walk is checkpointed per object, whatever a filter later keeps of it.
    fn entries<'a>(self, db: &ide::RootDatabaseImpl, config: &'a Configuration) -> Vec<Entry<'a>> {
        let mut entries = Vec::new();
        match self {
            Self::Objects(mdo_type) => {
                for object in checkpointed(db, config.metadata_objects()) {
                    if object.mdo_type == mdo_type {
                        entries.push(Entry::object(object));
                    }
                }
                for register in checkpointed(db, config.registers()) {
                    if register.mdo_type() == mdo_type {
                        entries.push(Entry::named(register.name()));
                    }
                }
            }
            Self::CommonModules => {
                for module in checkpointed(db, config.common_modules()) {
                    let line = format!("{} {}", module.name(), format_common_module_flags(module));
                    entries.push(Entry { line, ..Entry::named(module.name()) });
                }
            }
            Self::EventSubscriptions => {
                for subscription in checkpointed(db, config.event_subscriptions()) {
                    entries.push(Entry::named(subscription.name()));
                }
            }
            Self::DefinedTypes => {
                for defined_type in checkpointed(db, config.defined_types()) {
                    entries.push(Entry::named(defined_type.name()));
                }
            }
            Self::ScheduledJobs => {
                for job in checkpointed(db, config.scheduled_jobs()) {
                    entries.push(Entry::named(job.name()));
                }
            }
            Self::Roles => {
                for role in checkpointed(db, config.roles()) {
                    entries.push(Entry::named(role.name()));
                }
            }
            Self::Services(ServiceKind::Http) => {
                for service in checkpointed(db, config.http_services()) {
                    entries.push(Entry::named(service.name()));
                }
            }
            Self::Services(ServiceKind::Web) => {
                for service in checkpointed(db, config.web_services()) {
                    entries.push(Entry::named(service.name()));
                }
            }
            Self::Services(ServiceKind::Integration) => {
                for service in checkpointed(db, config.integration_services()) {
                    entries.push(Entry::named(service.name()));
                }
            }
        }
        entries
    }
}

/// The collections the tree lists apart from metadata objects and registers, by every folded
/// name an agent calls them; one name per arm keeps each line short.
fn collection_category(folded: &str) -> Option<Category> {
    Some(match folded {
        "общиймодуль" => Category::CommonModules,
        "общиемодули" => Category::CommonModules,
        "commonmodule" => Category::CommonModules,
        "commonmodules" => Category::CommonModules,
        "подписканасобытие" => Category::EventSubscriptions,
        "подпискинасобытия" => Category::EventSubscriptions,
        "eventsubscription" => Category::EventSubscriptions,
        "eventsubscriptions" => Category::EventSubscriptions,
        "определяемыйтип" => Category::DefinedTypes,
        "определяемыетипы" => Category::DefinedTypes,
        "definedtype" => Category::DefinedTypes,
        "definedtypes" => Category::DefinedTypes,
        "регламентноезадание" => Category::ScheduledJobs,
        "регламентныезадания" => Category::ScheduledJobs,
        "scheduledjob" => Category::ScheduledJobs,
        "scheduledjobs" => Category::ScheduledJobs,
        "роль" => Category::Roles,
        "роли" => Category::Roles,
        "role" => Category::Roles,
        "roles" => Category::Roles,
        _ => return None,
    })
}

/// One listed object: the names a filter is matched against and its line in the listing.
struct Entry<'a> {
    name: &'a str,
    english: Option<&'a str>,
    line: String,
}

impl<'a> Entry<'a> {
    fn named(name: &'a str) -> Self {
        Self { name, english: None, line: name.to_owned() }
    }

    fn object(object: &'a bsl_metadata::MetadataObject) -> Self {
        let line = match &object.name_en {
            Some(english) => format!("{} ({english})", object.name),
            None => object.name.clone(),
        };
        Self { name: &object.name, english: object.name_en.as_deref(), line }
    }
}

/// The configuration and each extension, with the heading of their part of a listing.
fn layers<'a>(
    config: &'a Configuration,
    extensions: &'a [(String, Configuration)],
) -> impl Iterator<Item = (String, &'a Configuration)> {
    std::iter::once(("Конфигурация".to_owned(), config))
        .chain(extensions.iter().map(|(name, layer)| (format!("Расширение: {name}"), layer)))
}

/// The note a cut listing ends with.
const LISTING_BUDGET_NOTE: &str = "\n-- список усечён под max_output_tokens; повысьте бюджет или \
                                   запросите объект действием `object` --\n";

/// One category's objects in the configuration and its extensions, narrowed to the names
/// containing `filter` when it is given. A known category with no object is listed empty; only
/// a name that is no category is refused.
fn format_filtered_tree(
    db: &ide::RootDatabaseImpl,
    config: &Configuration,
    extensions: &[(String, Configuration)],
    raw_category: &str,
    filter: Option<&str>,
    max_output_tokens: usize,
) -> Result<CallToolResult, McpError> {
    let Some(category) = Category::parse(raw_category) else {
        return Err(McpError::invalid_params(
            format!(
                "Категория '{raw_category}' не найдена. Список категорий: `metadata` с \
                 `action: \"tree\"` без `meta_type` и `filter`."
            ),
            None,
        ));
    };
    let needle = filter.map(stdx::case::fold_lower_per_char);
    let needle = needle.as_deref();
    let mut total = 0;
    let mut sections = String::new();
    for (title, layer) in layers(config, extensions) {
        let entries: Vec<_> = category
            .entries(db, layer)
            .into_iter()
            .filter(|entry| name_matches(needle, entry.name, entry.english))
            .collect();
        // Without extensions the listing keeps its flat shape: one heading, one line per object.
        if !extensions.is_empty() {
            if entries.is_empty() {
                continue;
            }
            let _ = writeln!(sections, "\n## {title} ({})", entries.len());
        }
        sections.push('\n');
        for entry in &entries {
            let _ = writeln!(sections, "- {}", entry.line);
        }
        total += entries.len();
    }

    let mut out = format!("# {} ({total})\n", category.heading());
    out.push_str(&sections);
    // A category listing on a large config (e.g. every Справочник in ERP) can be thousands of
    // lines; cap it to the output budget at a line boundary so the response stays bounded.
    crate::tools::response::truncate_text_to_budget(
        &mut out,
        max_output_tokens,
        LISTING_BUDGET_NOTE,
    );

    Ok(CallToolResult::success(vec![ContentBlock::text(out)]))
}

/// Every object of the configuration and of its extensions whose name contains `filter`,
/// grouped by the layer that holds it, one `Категория.Имя` line each: the pair the `object`
/// action takes.
fn format_name_matches(
    db: &ide::RootDatabaseImpl,
    config: &Configuration,
    extensions: &[(String, Configuration)],
    filter: &str,
    max_output_tokens: usize,
) -> CallToolResult {
    let needle = stdx::case::fold_lower_per_char(filter);
    let mut total = 0;
    let mut sections = String::new();
    for (title, layer) in layers(config, extensions) {
        let names = names_containing(db, layer, &needle);
        if names.is_empty() {
            continue;
        }
        total += names.len();
        let _ = writeln!(sections, "\n## {title} ({})\n", names.len());
        for name in names {
            let _ = writeln!(sections, "- {name}");
        }
    }

    let mut out = format!("# Объекты, в имени которых есть «{filter}» ({total})\n");
    // `filter` used to name a category, and a caller that still passes one gets objects whose
    // name merely contains it.
    if Category::parse(filter).is_some() {
        let _ = writeln!(
            out,
            "\n«{filter}» - это и имя категории: её объекты перечисляет `meta_type: \"{filter}\"`, \
             а `filter` ищет по части имени."
        );
    }
    if total == 0 {
        let _ = writeln!(
            out,
            "\nКатегорию задаёт `meta_type`; список категорий — `tree` без `meta_type` и `filter`."
        );
    }
    out.push_str(&sections);
    crate::tools::response::truncate_text_to_budget(
        &mut out,
        max_output_tokens,
        LISTING_BUDGET_NOTE,
    );
    CallToolResult::success(vec![ContentBlock::text(out)])
}

/// `Категория.Имя` of every object in `config` whose name, or English name, contains
/// `needle` (per-char lowercase), in the order the summary counts the categories.
fn names_containing(
    db: &ide::RootDatabaseImpl,
    config: &Configuration,
    needle: &str,
) -> Vec<String> {
    let mut names = Vec::new();
    for object in checkpointed(db, config.metadata_objects()) {
        if name_matches(Some(needle), &object.name, object.name_en.as_deref()) {
            names.push(format!("{}.{}", object.mdo_type.russian_name(), object.name));
        }
    }
    for register in checkpointed(db, config.registers()) {
        push_matching(&mut names, register.mdo_type().russian_name(), register.name(), needle);
    }
    for module in checkpointed(db, config.common_modules()) {
        push_matching(&mut names, "ОбщийМодуль", module.name(), needle);
    }
    for subscription in checkpointed(db, config.event_subscriptions()) {
        push_matching(&mut names, "ПодпискаНаСобытие", subscription.name(), needle);
    }
    for defined_type in checkpointed(db, config.defined_types()) {
        push_matching(&mut names, "ОпределяемыйТип", defined_type.name(), needle);
    }
    for job in checkpointed(db, config.scheduled_jobs()) {
        push_matching(&mut names, "РегламентноеЗадание", job.name(), needle);
    }
    for role in checkpointed(db, config.roles()) {
        push_matching(&mut names, "Роль", role.name(), needle);
    }
    for service in checkpointed(db, config.http_services()) {
        push_matching(&mut names, "HTTPСервис", service.name(), needle);
    }
    for service in checkpointed(db, config.web_services()) {
        push_matching(&mut names, "WebСервис", service.name(), needle);
    }
    for service in checkpointed(db, config.integration_services()) {
        push_matching(&mut names, "СервисИнтеграции", service.name(), needle);
    }
    names
}

fn push_matching(names: &mut Vec<String>, category: &str, name: &str, needle: &str) {
    if stdx::case::contains_ignore_case(name, needle) {
        names.push(format!("{category}.{name}"));
    }
}

/// Whether `name` or its English twin contains `needle` (per-char lowercase); no needle keeps
/// every name.
fn name_matches(needle: Option<&str>, name: &str, english: Option<&str>) -> bool {
    needle.is_none_or(|needle| {
        stdx::case::contains_ignore_case(name, needle)
            || english.is_some_and(|english| stdx::case::contains_ignore_case(english, needle))
    })
}

fn format_common_module_flags(m: &bsl_metadata::CommonModule) -> String {
    let mut flags = Vec::new();
    if m.is_server() {
        flags.push("Сервер");
    }
    if m.is_client_managed_application() {
        flags.push("Клиент");
    }
    if m.is_global() {
        flags.push("Глобальный");
    }
    if m.is_privileged() {
        flags.push("Привилегированный");
    }
    if m.is_server_call() {
        flags.push("ВызовСервера");
    }
    if flags.is_empty() {
        String::new()
    } else {
        format!("[{}]", flags.join(", "))
    }
}

/// The collections `object` reads from the configuration as a whole: a common module, a role,
/// a defined type, a scheduled job. The name search lists them as `Категория.Имя` too, so the
/// pair it prints has to open here. `None` for every other `object_type`, event subscriptions
/// included: they have a lookup of their own.
fn collection_object_kind(object_type: &str) -> Option<Category> {
    match collection_category(&object_type.trim().to_lowercase())? {
        Category::EventSubscriptions => None,
        category => Some(category),
    }
}

/// The card of the collection object `object_name`, from the first layer that has it.
fn collection_object_answer<'a>(
    mut layers: impl Iterator<Item = &'a Configuration>,
    category: Category,
    object_name: &str,
) -> Result<CallToolResult, McpError> {
    let card = layers.find_map(|config| collection_object_card(config, category, object_name));
    match card {
        Some(text) => Ok(CallToolResult::success(vec![ContentBlock::text(text)])),
        None => Err(McpError::invalid_params(
            format!("{}.{object_name} не найден в конфигурации", collection_kind_name(category)),
            None,
        )),
    }
}

fn collection_object_card(
    config: &Configuration,
    category: Category,
    name: &str,
) -> Option<String> {
    match category {
        Category::CommonModules => config.find_common_module(name).map(common_module_card),
        Category::Roles => config.find_role(name).map(role_card),
        Category::DefinedTypes => config.find_defined_type(name).map(defined_type_card),
        Category::ScheduledJobs => config.find_scheduled_job(name).map(scheduled_job_card),
        _ => None,
    }
}

fn collection_kind_name(category: Category) -> &'static str {
    match category {
        Category::CommonModules => "ОбщийМодуль",
        Category::Roles => "Роль",
        Category::DefinedTypes => "ОпределяемыйТип",
        Category::ScheduledJobs => "РегламентноеЗадание",
        _ => category.heading(),
    }
}

fn common_module_card(module: &bsl_metadata::CommonModule) -> String {
    let flags = format_common_module_flags(module);
    format!("# ОбщийМодуль.{}\n\nФлаги: {}\n", escape_markdown(module.name()), dash(&flags))
}

fn role_card(role: &bsl_metadata::Role) -> String {
    let objects = role.objects().len();
    format!("# Роль.{}\n\nОбъектов с правами: {objects}\n", escape_markdown(role.name()))
}

fn defined_type_card(defined_type: &bsl_metadata::DefinedType) -> String {
    let types = format_source_type(defined_type.underlying_type());
    format!("# ОпределяемыйТип.{}\n\nТип: {types}\n", escape_markdown(defined_type.name()))
}

fn scheduled_job_card(job: &bsl_metadata::ScheduledJob) -> String {
    let yes_no = |flag: bool| if flag { "да" } else { "нет" };
    format!(
        "# РегламентноеЗадание.{}\n\nМетод: {}\nПредопределённое: {}\nИспользование: {}\n",
        escape_markdown(job.name()),
        dash(job.method_name()),
        yes_no(job.is_predefined()),
        yes_no(job.is_enabled())
    )
}

/// The whole-`Configuration` object formatter. Production resolves objects from the
/// resident substrate via [`object_from_db`] (sharing the `format_*` renderers below);
/// this variant is retained as the fixture-driven contract test for that formatting, so
/// it is test-only.
#[cfg(test)]
pub fn get_object_structure(
    config: &Configuration,
    object_type: &str,
    object_name: &str,
) -> Result<CallToolResult, McpError> {
    if let Some(kind) = parse_service_kind(object_type) {
        return get_service_structure(config, kind, object_name);
    }
    if let Some(category) = collection_object_kind(object_type) {
        return collection_object_answer(std::iter::once(config), category, object_name);
    }

    let mdo_type: MdoType =
        object_type.parse().map_err(|e: String| McpError::invalid_params(e, None))?;

    // An event subscription is not a data-bearing object: it lives in its own catalog,
    // so it is looked up by name rather than via `find_metadata_object`/`find_register`.
    if mdo_type == MdoType::EventSubscription {
        return match config.find_event_subscription(object_name) {
            Some(sub) => Ok(CallToolResult::success(vec![ContentBlock::text(
                format_event_subscription_structure(sub),
            )])),
            None => Err(McpError::invalid_params(
                format!("ПодпискаНаСобытие.{} не найдена в конфигурации", object_name),
                None,
            )),
        };
    }

    if let Some(obj) = config.find_metadata_object(mdo_type, object_name) {
        return Ok(CallToolResult::success(vec![ContentBlock::text(
            format_metadata_object_structure(obj, mdo_type),
        )]));
    }

    if let Some(reg) = config.find_register_by_type_and_name(mdo_type, object_name) {
        return Ok(CallToolResult::success(vec![ContentBlock::text(format_register_structure(
            reg,
        ))]));
    }

    Err(McpError::invalid_params(
        format!("Объект {}.{} не найден в конфигурации", mdo_type.russian_name(), object_name),
        None,
    ))
}

#[cfg(test)]
fn get_service_structure(
    config: &Configuration,
    kind: ServiceKind,
    object_name: &str,
) -> Result<CallToolResult, McpError> {
    match kind {
        ServiceKind::Http => match config.find_http_service(object_name) {
            Some(service) => Ok(CallToolResult::success(vec![ContentBlock::text(
                format_http_service_structure(service),
            )])),
            None => Err(McpError::invalid_params(
                format!("HTTPService.{object_name} не найден в конфигурации"),
                None,
            )),
        },
        ServiceKind::Web => match config.find_web_service(object_name) {
            Some(service) => Ok(CallToolResult::success(vec![ContentBlock::text(
                format_web_service_structure(service),
            )])),
            None => Err(McpError::invalid_params(
                format!("WebService.{object_name} не найден в конфигурации"),
                None,
            )),
        },
        ServiceKind::Integration => match config.find_integration_service(object_name) {
            Some(service) => Ok(CallToolResult::success(vec![ContentBlock::text(
                format_integration_service_structure(service),
            )])),
            None => Err(McpError::invalid_params(
                format!("IntegrationService.{object_name} не найден в конфигурации"),
                None,
            )),
        },
    }
}

fn format_event_subscription_structure(sub: &bsl_metadata::EventSubscription) -> String {
    let mut out = format!("# ПодпискаНаСобытие.{}\n\n", sub.name());
    writeln!(out, "- Источник: {}", dash(sub.source())).ok();
    writeln!(out, "- Событие: {}", dash(sub.event())).ok();
    writeln!(out, "- Обработчик: {}", dash(sub.handler_string())).ok();
    if let Some(handler) = sub.parse_handler() {
        writeln!(out, "  - Модуль: {}", handler.module_name).ok();
        writeln!(out, "  - Метод: {}", handler.method_name).ok();
    }
    out
}

fn format_http_service_structure(service: &bsl_metadata::HTTPService) -> String {
    let mut out = format!("# HTTPService.{}\n\n", service.name());
    let _ = writeln!(out, "- RootURL: {}", dash(service.root_url()));
    if let Some(uri) = service.uri() {
        let _ = writeln!(out, "- Модуль: {uri}");
    }

    let templates = service.url_templates();
    if !templates.is_empty() {
        let _ = writeln!(out, "\n## URLTemplates ({})\n", templates.len());
        for template in templates {
            let _ = writeln!(out, "### {}", template.name());
            let _ = writeln!(out, "- URLTemplate: {}", dash(template.template()));
            let methods = template.methods();
            if !methods.is_empty() {
                let _ = writeln!(out, "\n| Метод | HTTPMethod | Обработчик |");
                let _ = writeln!(out, "|-------|------------|------------|");
                for method in methods {
                    let _ = writeln!(
                        out,
                        "| {} | {} | {} |",
                        method.name(),
                        dash(method.http_method()),
                        dash(method.handler())
                    );
                }
            }
            out.push('\n');
        }
    }

    out
}

fn format_web_service_structure(service: &bsl_metadata::WebService) -> String {
    let mut out = format!("# WebService.{}\n\n", service.name());
    let _ = writeln!(out, "- Namespace: {}", dash(service.namespace()));
    if let Some(uri) = service.uri() {
        let _ = writeln!(out, "- Модуль: {uri}");
    }

    let operations = service.operations();
    if !operations.is_empty() {
        let _ = writeln!(out, "\n## Операции ({})\n", operations.len());
        let _ = writeln!(out, "| Операция | Обработчик | Параметры |");
        let _ = writeln!(out, "|----------|------------|-----------|");
        for operation in operations {
            let parameters = operation
                .parameters()
                .iter()
                .map(bsl_metadata::WebServiceParameter::name)
                .collect::<Vec<_>>()
                .join(", ");
            let _ = writeln!(
                out,
                "| {} | {} | {} |",
                operation.name(),
                dash(operation.procedure_name()),
                dash(&parameters)
            );
        }
    }

    out
}

fn format_integration_service_structure(service: &bsl_metadata::IntegrationService) -> String {
    let mut out = format!("# IntegrationService.{}\n\n", service.name());
    let channels = service.channels();
    if !channels.is_empty() {
        let _ = writeln!(out, "## Каналы ({})\n", channels.len());
        let _ = writeln!(out, "| Канал | ReceiveMessageProcessing |");
        let _ = writeln!(out, "|-------|--------------------------|");
        for channel in channels {
            let _ = writeln!(
                out,
                "| {} | {} |",
                channel.name(),
                dash(channel.receive_message_processing())
            );
        }
    }

    out
}

fn format_metadata_object_structure(
    obj: &bsl_metadata::MetadataObject,
    mdo_type: MdoType,
) -> String {
    let mut out = format!("# {}.{}\n\n", mdo_type.russian_name(), escape_markdown(&obj.name));
    let unresolved = metadata_object_type_reasons(obj);

    if let Some(ref name_en) = obj.name_en {
        let _ = writeln!(out, "Английское имя: {}\n", escape_markdown(name_en));
    }

    append_type_completeness(&mut out, &unresolved);

    if !obj.attributes.is_empty() {
        let _ = writeln!(out, "## Реквизиты ({})\n", obj.attributes.len());
        let _ = writeln!(out, "| Имя | Тип |");
        let _ = writeln!(out, "|-----|-----|");
        for attr in &obj.attributes {
            let _ = writeln!(
                out,
                "| {} | {} |",
                escape_markdown(&attr.name),
                format_source_type(&attr.attr_type)
            );
        }
        out.push('\n');
    }

    if !obj.tabular_sections.is_empty() {
        let _ = writeln!(out, "## Табличные части ({})\n", obj.tabular_sections.len());
        for ts in &obj.tabular_sections {
            let _ = writeln!(out, "### {}\n", escape_markdown(ts.name()));
            if !ts.attributes().is_empty() {
                let _ = writeln!(out, "| Имя | Тип |");
                let _ = writeln!(out, "|-----|-----|");
                for attr in ts.attributes() {
                    let _ = writeln!(
                        out,
                        "| {} | {} |",
                        escape_markdown(attr.name()),
                        format_source_type(attr.attr_type())
                    );
                }
                out.push('\n');
            }
        }
    }

    if !obj.enum_values.is_empty() {
        let _ = writeln!(out, "## Значения перечисления ({})\n", obj.enum_values.len());
        for val in &obj.enum_values {
            if let Some(ref name_en) = val.name_en {
                let _ = writeln!(
                    out,
                    "- {} ({})",
                    escape_markdown(&val.name),
                    escape_markdown(name_en)
                );
            } else {
                let _ = writeln!(out, "- {}", escape_markdown(&val.name));
            }
        }
        out.push('\n');
    }

    if !obj.predefined_items.is_empty() {
        let _ = writeln!(out, "## Предопределённые элементы ({})\n", obj.predefined_items.len());
        for item in &obj.predefined_items {
            let _ = writeln!(out, "- {}", escape_markdown(&item.name));
        }
    }

    out
}

fn metadata_object_type_reasons(obj: &bsl_metadata::MetadataObject) -> Vec<(String, &'static str)> {
    let mut reasons = Vec::new();
    for attr in &obj.attributes {
        collect_type_reasons(&attr.attr_type, &format!("Реквизиты.{}", attr.name), &mut reasons);
    }
    for section in &obj.tabular_sections {
        for attr in section.attributes() {
            collect_type_reasons(
                attr.attr_type(),
                &format!("ТабличныеЧасти.{}.Реквизиты.{}", section.name(), attr.name()),
                &mut reasons,
            );
        }
    }
    reasons
}

fn collect_type_reasons(
    ty: &bsl_metadata::AttributeType,
    path: &str,
    reasons: &mut Vec<(String, &'static str)>,
) {
    use bsl_metadata::AttributeType;
    match ty {
        AttributeType::Unknown => reasons.push((path.to_string(), "technical_name_unavailable")),
        AttributeType::UnknownNamed(_) => {
            reasons.push((path.to_string(), "unknown_technical_name"))
        }
        AttributeType::Composite { types } => {
            for ty in types {
                collect_type_reasons(ty, path, reasons);
            }
        }
        _ => {}
    }
}

fn append_type_completeness(out: &mut String, reasons: &[(String, &'static str)]) {
    if reasons.is_empty() {
        out.push_str("Полнота типов: complete\n\n");
    } else {
        out.push_str("Полнота типов: partial\n");
        for (path, reason) in reasons {
            let _ = writeln!(out, "- {}: {reason}", escape_markdown(path));
        }
        out.push('\n');
    }
}

/// Markdown-safe by structure, not by presentation: object names are identifiers the
/// caller matches against the source configuration, so they travel verbatim — `_` and
/// `*` used to be escaped and made the names unmatchable (github#171). Only what would
/// corrupt the emitted structure is handled: the table cell separator (and the
/// backslash that could otherwise unescape it) and line breaks.
fn escape_markdown(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '|' => escaped.push_str("\\|"),
            '\n' | '\r' => escaped.push(' '),
            _ => escaped.push(ch),
        }
    }
    escaped
}

fn format_source_type(ty: &bsl_metadata::AttributeType) -> String {
    use bsl_metadata::AttributeType;

    match ty {
        AttributeType::Composite { types } => {
            types.iter().map(format_source_type).collect::<Vec<_>>().join("; ")
        }
        AttributeType::UnknownNamed(presentation) => {
            format!("Не разрешён: {} (technical_name_unavailable)", escape_markdown(presentation))
        }
        AttributeType::Unknown => {
            "Не разрешён: неизвестный вариант (technical_name_unavailable)".to_string()
        }
        other => escape_markdown(&other.to_string()),
    }
}

fn format_register_structure(reg: &bsl_metadata::Register) -> String {
    let mut out =
        format!("# {}.{}\n\n", reg.mdo_type().russian_name(), escape_markdown(reg.name()));
    let mut reasons = Vec::new();
    for dim in reg.dimensions() {
        collect_type_reasons(
            &register_type(dim.attr_type(), dim.type_str()),
            &format!("Измерения.{}", dim.name()),
            &mut reasons,
        );
    }
    for res in reg.resources() {
        collect_type_reasons(
            &register_type(res.attr_type(), res.type_str()),
            &format!("Ресурсы.{}", res.name()),
            &mut reasons,
        );
    }
    for attr in reg.attributes() {
        collect_type_reasons(
            &register_type(attr.attr_type(), attr.type_str()),
            &format!("Реквизиты.{}", attr.name()),
            &mut reasons,
        );
    }
    append_type_completeness(&mut out, &reasons);

    if !reg.dimensions().is_empty() {
        let _ = writeln!(out, "## Измерения ({})\n", reg.dimensions().len());
        let _ = writeln!(out, "| Имя | Тип |");
        let _ = writeln!(out, "|-----|-----|");
        for dim in reg.dimensions() {
            let type_str = format_source_type(&register_type(dim.attr_type(), dim.type_str()));
            let _ = writeln!(out, "| {} | {} |", escape_markdown(dim.name()), type_str);
        }
        out.push('\n');
    }

    if !reg.resources().is_empty() {
        let _ = writeln!(out, "## Ресурсы ({})\n", reg.resources().len());
        let _ = writeln!(out, "| Имя | Тип |");
        let _ = writeln!(out, "|-----|-----|");
        for res in reg.resources() {
            let type_str = format_source_type(&register_type(res.attr_type(), res.type_str()));
            let _ = writeln!(out, "| {} | {} |", escape_markdown(res.name()), type_str);
        }
        out.push('\n');
    }

    if !reg.attributes().is_empty() {
        let _ = writeln!(out, "## Реквизиты ({})\n", reg.attributes().len());
        let _ = writeln!(out, "| Имя | Тип |");
        let _ = writeln!(out, "|-----|-----|");
        for attr in reg.attributes() {
            let type_str = format_source_type(&register_type(attr.attr_type(), attr.type_str()));
            let _ = writeln!(out, "| {} | {} |", escape_markdown(attr.name()), type_str);
        }
        out.push('\n');
    }

    if let Some(periodicity) = reg.periodicity() {
        let _ = writeln!(out, "Периодичность: {periodicity:?}");
    }
    if let Some(register_type) = reg.register_type() {
        let _ = writeln!(out, "Вид регистра: {register_type:?}");
    }

    out
}

fn register_type(
    parsed: Option<&bsl_metadata::AttributeType>,
    source: &str,
) -> bsl_metadata::AttributeType {
    parsed.cloned().unwrap_or_else(|| {
        if source.is_empty() {
            bsl_metadata::AttributeType::Unknown
        } else {
            bsl_metadata::AttributeType::UnknownNamed(source.to_string())
        }
    })
}

pub fn get_configuration_info(
    config: &Configuration,
    extensions: &[(String, Configuration)],
) -> Result<CallToolResult, McpError> {
    let total_objects = config.metadata_objects().len()
        + config.registers().len()
        + config.common_modules().len()
        + config.event_subscriptions().len()
        + config.defined_types().len()
        + config.scheduled_jobs().len()
        + config.roles().len()
        + config.http_services().len()
        + config.web_services().len()
        + config.integration_services().len();

    let mut out = format!("# Конфигурация: {}\n\n", config.name());
    let _ = writeln!(out, "- UUID: {}", config.uuid());
    let _ = writeln!(out, "- Всего объектов метаданных: {total_objects}");
    let _ = writeln!(out, "- Общие модули: {}", config.common_modules().len());
    let _ = writeln!(out, "- Объекты метаданных: {}", config.metadata_objects().len());
    let _ = writeln!(out, "- Регистры: {}", config.registers().len());
    let _ = writeln!(out, "- Подписки на события: {}", config.event_subscriptions().len());
    let _ = writeln!(out, "- Определяемые типы: {}", config.defined_types().len());
    let _ = writeln!(out, "- Регламентные задания: {}", config.scheduled_jobs().len());
    let _ = writeln!(out, "- Роли: {}", config.roles().len());
    let _ = writeln!(out, "- HTTP-сервисы: {}", config.http_services().len());
    let _ = writeln!(out, "- Web-сервисы: {}", config.web_services().len());
    let _ = writeln!(out, "- Сервисы интеграции: {}", config.integration_services().len());

    if !extensions.is_empty() {
        let _ = writeln!(out, "\n## Расширения ({})\n", extensions.len());
        for (name, ext_config) in extensions {
            let ext_objects = ext_config.metadata_objects().len()
                + ext_config.registers().len()
                + ext_config.common_modules().len();
            let _ = writeln!(out, "- **{name}**: {ext_objects} объектов");
        }
    }

    Ok(CallToolResult::success(vec![ContentBlock::text(out)]))
}

/// Read forms from disk. `source_root` MUST be the configuration root (the
/// `Configuration.xml`-bearing directory, e.g. `src/cf`) — passing the repo root when the
/// configuration is nested under `src/cf` makes every form look missing.
///
/// `object_name` is required for an object's forms (`<TypeDir>/<object>/Forms/…`) but ignored
/// for `CommonForm`, which is a top-level form with no parent object (`CommonForms/<Form>/…`).
pub fn get_form_structure(
    source_root: Option<&std::path::Path>,
    object_type: &str,
    object_name: Option<&str>,
    form_name: Option<&str>,
) -> Result<CallToolResult, McpError> {
    let root = source_root.ok_or_else(|| {
        McpError::invalid_params("Configuration root не задан, формы недоступны", None)
    })?;

    // CommonForm has no parent object: its forms live directly under `CommonForms/<Form>`,
    // not `<TypeDir>/<object>/Forms/<Form>`, so it takes a distinct path and no `object_name`.
    // `to_lowercase` (not `eq_ignore_ascii_case`) so the Cyrillic alias folds case too.
    if matches!(object_type.to_lowercase().as_str(), "commonform" | "общаяформа") {
        let common_forms = bsl_conventions::find_child_ci(root, "CommonForms")
            .unwrap_or_else(|| root.join("CommonForms"));
        return forms_in_container(&common_forms, form_name, "ОбщаяФорма");
    }

    let object_name = object_name.ok_or_else(|| {
        McpError::invalid_params(
            "'object_name' обязателен для форм объекта (кроме CommonForm)",
            None,
        )
    })?;
    let type_dir = mdo_type_to_dir(object_type).ok_or_else(|| {
        McpError::invalid_params(format!("Неизвестный тип объекта: {object_type}"), None)
    })?;

    let type_root =
        bsl_conventions::find_child_ci(root, type_dir).unwrap_or_else(|| root.join(type_dir));
    let object_dir = type_root.join(object_name);
    let forms_dir = bsl_conventions::find_child_ci(
        &object_dir,
        bsl_conventions::ConventionalName::Forms.canonical(),
    )
    .unwrap_or_else(|| object_dir.join(bsl_conventions::ConventionalName::Forms.canonical()));
    forms_in_container(&forms_dir, form_name, &format!("{object_type}.{object_name}"))
}

/// Read a single form (when `form_name` is given) or list every form directory inside
/// `container` (`.../Forms` for an object, `CommonForms` for a common form). `title` labels
/// the listing header and the empty-set error.
fn forms_in_container(
    container: &std::path::Path,
    form_name: Option<&str>,
    title: &str,
) -> Result<CallToolResult, McpError> {
    if !container.exists() {
        return Err(McpError::invalid_params(
            format!("Каталог форм не найден: {}", container.display()),
            None,
        ));
    }

    if let Some(fname) = form_name {
        let form_dir = container.join(fname);
        let form_xml_path = bsl_conventions::resolve_chain_ci(
            &form_dir,
            &[
                bsl_conventions::ConventionalName::Ext.canonical(),
                bsl_conventions::ConventionalName::FormXml.canonical(),
            ],
        )
        .unwrap_or_else(|| {
            form_dir
                .join(bsl_conventions::ConventionalName::Ext.canonical())
                .join(bsl_conventions::ConventionalName::FormXml.canonical())
        });
        if !form_xml_path.exists() {
            return Err(McpError::invalid_params(
                format!("Форма не найдена: {}", form_xml_path.display()),
                None,
            ));
        }
        let xml = std::fs::read_to_string(&form_xml_path)
            .map_err(|e| McpError::internal_error(format!("Ошибка чтения формы: {e}"), None))?;
        let form = bsl_metadata::xml_parser::parse_form_xml(&xml)
            .map_err(|e| McpError::internal_error(format!("Ошибка разбора формы: {e}"), None))?;
        Ok(CallToolResult::success(vec![ContentBlock::text(format_form(&form, Some(fname)))]))
    } else {
        let mut form_names = Vec::new();
        if let Ok(entries) = std::fs::read_dir(container) {
            for entry in entries.flatten() {
                if entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false) {
                    if let Some(name) = entry.file_name().to_str() {
                        form_names.push(name.to_string());
                    }
                }
            }
        }
        form_names.sort();

        if form_names.is_empty() {
            return Err(McpError::invalid_params(format!("Формы не найдены для {title}"), None));
        }

        let mut out = format!("# Формы {title}\n\n");
        for name in &form_names {
            let _ = writeln!(out, "- {name}");
        }
        let _ =
            writeln!(out, "\nИспользуйте `form_name` для получения структуры конкретной формы.");
        Ok(CallToolResult::success(vec![ContentBlock::text(out)]))
    }
}

/// Render a parsed form. `requested_name` is the form name the caller asked for: `Ext/Form.xml`
/// holds the managed-form *content* (items, attributes, handlers) but not the form's own name or
/// UUID — those live in the parent object's metadata — so the parsed name is usually empty and
/// the UUID nil. Fall back to the requested name for the heading, and label a nil UUID as
/// unavailable instead of printing a misleading all-zero one.
fn format_form(form: &bsl_metadata::Form, requested_name: Option<&str>) -> String {
    let title = match (form.name(), requested_name) {
        (name, _) if !name.is_empty() => name,
        (_, Some(requested)) => requested,
        (_, None) => "<имя недоступно>",
    };
    let mut out = format!("# Форма: {title}\n\n");
    let _ = writeln!(out, "- Тип: {:?}", form.form_type());
    let uuid = form.uuid();
    if uuid.is_nil() {
        let _ = writeln!(out, "- UUID: недоступен (хранится в метаданных объекта, не в Form.xml)");
    } else {
        let _ = writeln!(out, "- UUID: {uuid}");
    }

    if !form.attributes.is_empty() {
        let _ = writeln!(out, "\n## Реквизиты формы ({})\n", form.attributes.len());
        for attr in &form.attributes {
            let main_marker = if attr.is_main { " (основной)" } else { "" };
            let _ = writeln!(out, "- {}{}: {}", attr.name, main_marker, attr.attr_type);
            for col in &attr.columns {
                let _ = writeln!(out, "    - {}: {}", col.name, col.attr_type);
            }
        }
    }

    let elements = form.elements();
    if !elements.is_empty() {
        let _ = writeln!(out, "\n## Элементы ({})\n", elements.len());
        let _ = writeln!(out, "| Имя | DataPath |");
        let _ = writeln!(out, "|-----|----------|");
        for el in elements {
            let dp = el.data_path.as_deref().unwrap_or("—");
            let _ = writeln!(out, "| {} | {dp} |", el.name);
        }
    }

    if !form.event_handlers.is_empty() {
        let _ = writeln!(out, "\n## Обработчики событий ({})\n", form.event_handlers.len());
        for h in &form.event_handlers {
            let _ = writeln!(out, "- {}", h.handler_name);
        }
    }

    if !form.command_handlers.is_empty() {
        let _ = writeln!(out, "\n## Обработчики команд ({})\n", form.command_handlers.len());
        for h in &form.command_handlers {
            let _ = writeln!(out, "- {h}");
        }
    }

    out
}

fn format_extension_summary(
    db: &ide::RootDatabaseImpl,
    name: &str,
    config: &Configuration,
) -> String {
    use std::collections::BTreeMap;
    let mut out = format!("\n---\n\n# Расширение: {name}\n\n");
    let mut categories: BTreeMap<&str, usize> = BTreeMap::new();

    for obj in checkpointed(db, config.metadata_objects()) {
        let key = obj.mdo_type.russian_name();
        *categories.entry(key).or_default() += 1;
    }
    for reg in checkpointed(db, config.registers()) {
        let key = reg.mdo_type().russian_name();
        *categories.entry(key).or_default() += 1;
    }
    let common_modules = config.common_modules().len();
    if common_modules > 0 {
        categories.insert("ОбщийМодуль", common_modules);
    }

    let total: usize = categories.values().sum();
    let _ = writeln!(out, "Всего объектов: {total}\n");
    let _ = writeln!(out, "| Категория | Количество |");
    let _ = writeln!(out, "|-----------|------------|");
    for (category, count) in &categories {
        let _ = writeln!(out, "| {category} | {count} |");
    }
    out
}

/// Whether the `object` action can resolve `object_type`, mirroring [`object_from_db`]'s
/// dispatch order: a service kind (`parse_service_kind`) FIRST, then an [`MdoType`]. The
/// force-rescan miss-retry gate uses this so it cannot drift from what the resolver
/// actually accepts — `MdoType::from_str` has NO service variants, so gating on it alone
/// would never retry a just-added HTTP/Web/Integration service (they'd stay "not found"
/// until the next periodic drift poll, unlike catalogs/registers/subscriptions). The
/// collections `object` reads whole (common modules, roles, defined types, scheduled jobs)
/// are resolvable as well.
pub(crate) fn is_resolvable_object_type(object_type: &str) -> bool {
    parse_service_kind(object_type).is_some()
        || collection_object_kind(object_type).is_some()
        || object_type.parse::<MdoType>().is_ok()
}

/// The `metadata object` action reading the resident host: resolve the object across the
/// base configuration and every extension (a point-lookup on the per-MDO substrate, never
/// a whole-config load) and format it. The db counterpart of [`get_object_structure`];
/// they share the `format_*` renderers. Extension-merged visibility, so an object defined
/// only in an extension is found (wider than the retired base-only read).
pub fn object_from_db(
    db: &ide::RootDatabaseImpl,
    object_type: &str,
    object_name: &str,
) -> Result<CallToolResult, McpError> {
    if let Some(kind) = parse_service_kind(object_type) {
        return service_from_db(db, kind, object_name);
    }
    // These collections have no per-object lookup on the substrate, so they are read from the
    // configuration and its extensions as a whole.
    if let Some(category) = collection_object_kind(object_type) {
        let (config, extensions) = configs_from_db(db);
        let layers = std::iter::once(&config).chain(extensions.iter().map(|(_, layer)| layer));
        return collection_object_answer(layers, category, object_name);
    }

    let mdo_type: MdoType =
        object_type.parse().map_err(|e: String| McpError::invalid_params(e, None))?;

    if mdo_type == MdoType::EventSubscription {
        return match db.resolve_event_subscription_across_roots(object_name) {
            Some(sub) => Ok(CallToolResult::success(vec![ContentBlock::text(
                format_event_subscription_structure(&sub),
            )])),
            None => Err(McpError::invalid_params(
                format!("ПодпискаНаСобытие.{object_name} не найдена в конфигурации"),
                None,
            )),
        };
    }

    if let Some(obj) = db.resolve_metadata_object_across_roots(mdo_type, object_name) {
        return Ok(CallToolResult::success(vec![ContentBlock::text(
            format_metadata_object_structure(&obj, mdo_type),
        )]));
    }

    if let Some(reg) = db.resolve_register_across_roots(mdo_type, object_name) {
        return Ok(CallToolResult::success(vec![ContentBlock::text(format_register_structure(
            &reg,
        ))]));
    }

    Err(McpError::invalid_params(
        format!("Объект {}.{} не найден в конфигурации", mdo_type.russian_name(), object_name),
        None,
    ))
}

fn service_from_db(
    db: &ide::RootDatabaseImpl,
    kind: ServiceKind,
    object_name: &str,
) -> Result<CallToolResult, McpError> {
    match kind {
        ServiceKind::Http => match db.resolve_http_service_across_roots(object_name) {
            Some(service) => Ok(CallToolResult::success(vec![ContentBlock::text(
                format_http_service_structure(&service),
            )])),
            None => Err(McpError::invalid_params(
                format!("HTTPService.{object_name} не найден в конфигурации"),
                None,
            )),
        },
        ServiceKind::Web => match db.resolve_web_service_across_roots(object_name) {
            Some(service) => Ok(CallToolResult::success(vec![ContentBlock::text(
                format_web_service_structure(&service),
            )])),
            None => Err(McpError::invalid_params(
                format!("WebService.{object_name} не найден в конфигурации"),
                None,
            )),
        },
        ServiceKind::Integration => {
            match db.resolve_integration_service_across_roots(object_name) {
                Some(service) => Ok(CallToolResult::success(vec![ContentBlock::text(
                    format_integration_service_structure(&service),
                )])),
                None => Err(McpError::invalid_params(
                    format!("IntegrationService.{object_name} не найден в конфигурации"),
                    None,
                )),
            }
        }
    }
}

/// The base configuration plus each extension's, sourced from the resident db's Channel-2
/// `load_configuration` query (Salsa-cached, invalidated by metadata drift). The `tree`
/// and `info` enumeration payload — which inherently needs whole-config counts — after
/// the `MetadataCache` retirement. `object` never calls this (it stays on the substrate).
pub fn configs_from_db(
    db: &ide::RootDatabaseImpl,
) -> (Configuration, Vec<(String, Configuration)>) {
    let paths = db.designer_config_paths();
    let base = paths
        .iter()
        .find(|(label, _)| label.is_none())
        .map(|(_, p)| (*db.configuration_for_root(p)).clone())
        .unwrap_or_else(|| Configuration::new("Configuration"));
    let extensions = paths
        .iter()
        .filter_map(|(label, p)| {
            label.as_ref().map(|name| (name.clone(), (*db.configuration_for_root(p)).clone()))
        })
        .collect();
    (base, extensions)
}

/// A "still loading, retry shortly" envelope for a metadata call issued while the resident
/// db is building (or rebuilding after an idle eviction). Never a hard "not loaded" error —
/// the resident becomes ready, OR fails and is retried on its own backoff, so the answer is
/// always either "ask again in a moment" or "here is why it stopped and when the next try
/// comes". A `status` of `failed` (with `error` and `retry_after_ms`) is a build that did not
/// finish; it is not an eternal wait, and it is not terminal either.
///
/// The machine-readable body has the same shape `diagnostics` returns — only `detail`
/// differs, naming this tool's view of the one build — so "not ready" is recognized by one
/// field check across every tool reading the resident. The Russian
/// sentence stays the text block instead of the JSON mirror `graph`/`diagnostics` emit:
/// `metadata` is a text tool (its answers are listings, not JSON), and a consumer that
/// matched the sentence before the envelope existed keeps working while it migrates to
/// `structuredContent`.
pub fn loading(report: &crate::diagnostics_state::StatusReport) -> CallToolResult {
    let mut msg = String::from("Метаданные загружаются, повторите запрос через несколько секунд.");
    if let Some(ms) = report.elapsed_ms {
        let _ = write!(msg, " (идёт загрузка, {ms} мс)");
    }
    crate::tools::response::structured_with_text(
        msg,
        crate::tools::resident::loading_body(
            report,
            "resident analysis database is building; retry shortly",
        ),
    )
}

#[cfg(test)]
fn fixture_config() -> Configuration {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-metadata/fixtures/designer");
    bsl_metadata::load_from_directory(path).expect("failed to load fixture configuration")
}

fn mdo_type_to_dir(object_type: &str) -> Option<&'static str> {
    match object_type.to_lowercase().as_str() {
        "catalog" | "справочник" => Some("Catalogs"),
        "document" | "документ" => Some("Documents"),
        "dataprocessor" | "обработка" => Some("DataProcessors"),
        "report" | "отчет" | "отчёт" => Some("Reports"),
        "enum" | "перечисление" => Some("Enums"),
        "chartofcharacteristictypes" | "планвидовхарактеристик" => {
            Some("ChartsOfCharacteristicTypes")
        }
        "chartofaccounts" | "плансчетов" => Some("ChartsOfAccounts"),
        "exchangeplan" | "планобмена" => Some("ExchangePlans"),
        "businessprocess" | "бизнеспроцесс" => Some("BusinessProcesses"),
        "task" | "задача" => Some("Tasks"),
        "eventsubscription" | "подписканасобытие" => Some("EventSubscriptions"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extract_text(result: &CallToolResult) -> &str {
        result.content[0].as_text().expect("expected text content").text.as_str()
    }

    #[test]
    fn source_metadata_renders_each_union_variant_and_marks_unresolved_types() {
        fn config_with_union(type_entries: &str) -> bsl_metadata::Configuration {
            let dir = tempfile::tempdir().expect("source fixture tempdir");
            std::fs::create_dir_all(dir.path().join("Catalogs")).expect("Catalogs directory");
            std::fs::write(
                dir.path().join("Configuration.xml"),
                "<Configuration><Name>ТестоваяКонфигурация</Name></Configuration>",
            )
            .expect("Configuration.xml");

            let catalog = |name: &str, uuid: &str, children: &str| {
                format!(
                    r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:v8="http://v8.1c.ru/8.1/data/core" xmlns:cfg="http://v8.1c.ru/8.1/data/enterprise/current-config" version="2.10">
  <Catalog uuid="{uuid}">
    <Properties><Name>{name}</Name><Synonym><v8:item><v8:lang>ru</v8:lang><v8:content>Общее представление</v8:content></v8:item></Synonym></Properties>
    <ChildObjects>{children}</ChildObjects>
  </Catalog>
</MetaDataObject>"#
                )
            };
            for (name, uuid) in [
                ("А", "00000000-0000-0000-0000-000000000001"),
                ("Б", "00000000-0000-0000-0000-000000000002"),
            ] {
                let xml = catalog(name, uuid, "");
                std::fs::write(dir.path().join("Catalogs").join(format!("{name}.xml")), xml)
                    .expect("write target catalog");
            }
            let attribute = format!(
                r#"<Attribute uuid="00000000-0000-0000-0000-000000000004"><Properties><Name>Вариант</Name><Synonym/><Type>{type_entries}</Type></Properties></Attribute>"#
            );
            let xml =
                catalog("ТестовыйКаталог", "00000000-0000-0000-0000-000000000003", &attribute);
            std::fs::write(dir.path().join("Catalogs/ТестовыйКаталог.xml"), xml)
                .expect("write catalog with union");
            bsl_metadata::load_from_directory(dir.path()).expect("load source fixture")
        }

        let complete_config = config_with_union(
            "<v8:Type>cfg:CatalogRef.А</v8:Type><v8:Type>cfg:CatalogRef.Б</v8:Type>",
        );
        let complete =
            get_object_structure(&complete_config, "Справочник", "ТестовыйКаталог").unwrap();
        let complete = extract_text(&complete);
        assert!(complete.contains("| Вариант | Справочник.А; Справочник.Б |"), "{complete}");
        assert!(complete.contains("Полнота типов: complete"), "{complete}");

        let partial_config = config_with_union(
            "<v8:Type>cfg:CatalogRef.А</v8:Type><v8:Type>d5p1:FutureType</v8:Type>",
        );
        let partial =
            get_object_structure(&partial_config, "Справочник", "ТестовыйКаталог").unwrap();
        let partial = extract_text(&partial);
        assert!(
            partial.contains("| Вариант | Справочник.А; Не разрешён: d5p1:FutureType (technical_name_unavailable) |"),
            "{partial}"
        );
        assert!(
            partial.contains("Полнота типов: partial\n- Реквизиты.Вариант: unknown_technical_name"),
            "{partial}"
        );
    }

    /// Имена — идентификаторы для последующих вызовов, и `_`/`*` в них не
    /// экранируются; экранирование осталось только структурным (github#171).
    #[test]
    fn names_travel_verbatim_and_only_structure_is_escaped() {
        assert_eq!(
            escape_markdown("Номенклатура_С_Подчёркиванием"),
            "Номенклатура_С_Подчёркиванием"
        );
        assert_eq!(escape_markdown("Тип*Звёздочка"), "Тип*Звёздочка");
        assert_eq!(escape_markdown("a|b"), "a\\|b");
        // Обратный слэш перед `|` обязан удвоиться: иначе `\|` из имени
        // «разэкранировался» бы и пайп разорвал ячейку.
        assert_eq!(escape_markdown("a\\|b"), "a\\\\\\|b");
        assert_eq!(escape_markdown("a\nb"), "a b");
        assert_eq!(escape_markdown("a\\b"), "a\\\\b");
    }

    /// Подчёркивание в имени объекта и реквизита доезжает до вывода дословно:
    /// агент сопоставляет эти имена с исходной конфигурацией и не должен
    /// разбирать экранирование (github#171).
    #[test]
    fn object_and_attribute_names_with_underscores_stay_verbatim() {
        use bsl_metadata::metadata_object::MdoType;
        use bsl_metadata::{Attribute, AttributeType, Configuration, MetadataObject};

        let mut config = Configuration::new("Тест");
        let mut object = MetadataObject::new(MdoType::Catalog, "Номенклатура_С_Подчёркиванием");
        object.add_attribute(Attribute {
            name: "Характеристика_НДС".into(),
            name_en: None,
            attr_type: AttributeType::String { length: Some(20) },
        });
        config.add_metadata_object(object);

        let result =
            get_object_structure(&config, "Справочник", "Номенклатура_С_Подчёркиванием").unwrap();
        let text = extract_text(&result);

        assert!(text.contains("# Справочник.Номенклатура_С_Подчёркиванием"), "{text}");
        assert!(text.contains("Характеристика_НДС"), "{text}");
        assert!(!text.contains('\\'), "имена обязаны ехать дословно: {text}");
    }

    #[test]
    fn source_type_completeness_uses_qualified_paths_for_sections_and_registers() {
        use bsl_metadata::{
            metadata_object::MdoType,
            register::{Register, RegisterResource},
            tabular_section::{TabularSection, TabularSectionAttribute},
            AttributeType, Configuration, MetadataObject,
        };

        let mut config = Configuration::new("Тест");
        let mut object = MetadataObject::new(MdoType::Catalog, "Справочник");
        for (uuid, section_name) in [
            ("00000000-0000-0000-0000-000000000001", "Товары"),
            ("00000000-0000-0000-0000-000000000002", "Услуги"),
        ] {
            let mut section = TabularSection::new(uuid.parse().unwrap(), section_name);
            section.set_attributes(vec![TabularSectionAttribute::new(
                uuid.parse().unwrap(),
                "Сумма|Итого",
                AttributeType::Unknown,
            )]);
            object.add_tabular_section(section);
        }
        config.add_metadata_object(object);

        let mut unknown_resource = RegisterResource::new(
            "00000000-0000-0000-0000-000000000004".parse().unwrap(),
            "Будущий|Ресурс",
        );
        unknown_resource.set_attr_type(AttributeType::UnknownNamed("future|type".into()));
        let mut known_resource = RegisterResource::new(
            "00000000-0000-0000-0000-000000000005".parse().unwrap(),
            "Количество",
        );
        known_resource.set_attr_type(AttributeType::String { length: Some(20) });
        config.add_register(
            Register::builder()
                .name("Продажи")
                .mdo_type(MdoType::InformationRegister)
                .add_resource(known_resource)
                .add_resource(unknown_resource)
                .build(),
        );

        let object_text = get_object_structure(&config, "Справочник", "Справочник").unwrap();
        let object_text = extract_text(&object_text);
        assert!(object_text.contains("Полнота типов: partial\n- ТабличныеЧасти.Товары.Реквизиты.Сумма\\|Итого: technical_name_unavailable\n- ТабличныеЧасти.Услуги.Реквизиты.Сумма\\|Итого: technical_name_unavailable"), "{object_text}");
        assert!(
            object_text.find("Полнота типов").unwrap()
                < object_text.find("## Табличные части").unwrap(),
            "{object_text}"
        );
        assert!(
            object_text.contains(
                "| Сумма\\|Итого | Не разрешён: неизвестный вариант (technical_name_unavailable) |"
            ),
            "{object_text}"
        );

        let register_text = get_object_structure(&config, "РегистрСведений", "Продажи").unwrap();
        let register_text = extract_text(&register_text);
        assert!(
            register_text.contains(
                "Полнота типов: partial\n- Ресурсы.Будущий\\|Ресурс: unknown_technical_name"
            ),
            "{register_text}"
        );
        assert!(
            register_text.find("Полнота типов").unwrap()
                < register_text.find("## Ресурсы").unwrap(),
            "{register_text}"
        );
        assert!(
            register_text.contains(
                "| Будущий\\|Ресурс | Не разрешён: future\\|type (technical_name_unavailable) |"
            ),
            "{register_text}"
        );
        assert!(register_text.contains("| Количество | Строка(20) |"), "{register_text}");
    }

    /// Every listing a cancelled `metadata` call can enter stops at its first item
    /// instead of enumerating the configuration for a response nobody will read.
    ///
    /// One filter per branch, because the branches name different collections and a
    /// checkpoint on one says nothing about the next. What a pre-cancelled token cannot
    /// reach is a SECOND loop on the same path — the first one unwinds before it — so
    /// the summary's register pass and the extension summary are covered by entry only;
    /// they walk the same iterator, and it is the iterator that carries the checkpoint.
    #[test]
    fn a_cancelled_listing_stops_at_the_first_item() {
        let config = fixture_config();
        let db = ide::RootDatabaseImpl::new();
        salsa::Database::cancellation_token(&db).cancel();

        // Each entry names the collection the branch walks; a branch whose collection is
        // empty in the fixture would answer an empty listing and the assertion says so.
        for category in [
            None,
            Some("Справочник"),
            Some("РегистрСведений"),
            Some("ОбщийМодуль"),
            Some("ПодпискаНаСобытие"),
            Some("ПодпискиНаСобытия"),
            Some("Роль"),
            Some("Роли"),
            Some("РегламентноеЗадание"),
            Some("HTTPСервис"),
            Some("WebСервис"),
            Some("СервисИнтеграции"),
        ] {
            let category = category.map(str::to_string);
            let got = salsa::Cancelled::catch(std::panic::AssertUnwindSafe(|| {
                get_metadata_tree(&db, &config, &[], category.clone(), None, usize::MAX)
            }));
            assert!(
                matches!(got, Err(salsa::Cancelled::Local)),
                "category {category:?} rendered its whole listing after the request was cancelled"
            );
        }

        // A name search walks every collection of every layer, and a filter narrows a
        // category only after the walk has passed its checkpoint.
        let got = salsa::Cancelled::catch(std::panic::AssertUnwindSafe(|| {
            get_metadata_tree(&db, &config, &[], None, Some("а".to_string()), usize::MAX)
        }));
        assert!(
            matches!(got, Err(salsa::Cancelled::Local)),
            "the name search walked every object after the request was cancelled"
        );
        let extensions = vec![("Расширение".to_owned(), fixture_config())];
        let empty = Configuration::new("Пустая");
        let got = salsa::Cancelled::catch(std::panic::AssertUnwindSafe(|| {
            let category = Some("Роль".to_string());
            let filter = Some("НетТакогоИмени".to_string());
            get_metadata_tree(&db, &empty, &extensions, category, filter, usize::MAX)
        }));
        assert!(
            matches!(got, Err(salsa::Cancelled::Local)),
            "a filtered category walked an extension's roles after the request was cancelled"
        );

        let got = salsa::Cancelled::catch(std::panic::AssertUnwindSafe(|| {
            format_extension_summary(&db, "Расширение", &config)
        }));
        assert!(
            matches!(got, Err(salsa::Cancelled::Local)),
            "the extension summary counted every object after the request was cancelled"
        );

        // The SELECTION of a category with no objects, on a configuration with no
        // registers either: every later loop is empty, so this input can only be
        // answered by the checkpoint on the selection itself. On the full fixture the
        // register selection unwinds first and hides whether this one exists at all.
        let objects_only = objects_only_config();
        let got = salsa::Cancelled::catch(std::panic::AssertUnwindSafe(|| {
            get_metadata_tree(
                &db,
                &objects_only,
                &[],
                Some("ПланОбмена".to_string()),
                None,
                usize::MAX,
            )
        }));
        assert!(
            matches!(got, Err(salsa::Cancelled::Local)),
            "the category selection walked every object after the request was cancelled"
        );
    }

    /// The designer fixture with its registers left out — see the test above for why
    /// their absence is the point.
    fn objects_only_config() -> Configuration {
        let dir = tempfile::tempdir().expect("tempdir");
        let fixture = std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../bsl-metadata/fixtures/designer"
        ));
        std::fs::copy(fixture.join("Configuration.xml"), dir.path().join("Configuration.xml"))
            .expect("copy");
        copy_tree(&fixture.join("Catalogs"), &dir.path().join("Catalogs"));
        let config = bsl_metadata::load_from_directory(dir.path()).expect("load");
        assert!(
            config.registers().is_empty() && !config.metadata_objects().is_empty(),
            "the stand must have objects and no registers, or it proves nothing"
        );
        config
    }

    fn copy_tree(from: &std::path::Path, to: &std::path::Path) {
        std::fs::create_dir_all(to).expect("mkdir");
        for entry in std::fs::read_dir(from).expect("read_dir").flatten() {
            let target = to.join(entry.file_name());
            if entry.file_type().expect("file_type").is_dir() {
                copy_tree(&entry.path(), &target);
            } else {
                std::fs::copy(entry.path(), target).expect("copy");
            }
        }
    }

    #[test]
    fn live_object_serializes_versioned_normalized_variants() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../bsl-analyzer/tests/fixtures/live_metadata_type_variants.json"
        ))
        .unwrap();
        let object: onec_client::MetadataStructureResult =
            serde_json::from_value(fixture["ru"].clone()).unwrap();
        let result = live_metadata_object_response(Some("test"), object, 6_000).unwrap();
        let body = result.structured_content.unwrap();

        assert_eq!(body["schema_version"], "2");
        assert_eq!(body["source"], "infobase");
        assert_eq!(body["connection"], "test");
        assert!(body["object"].get("futureField").is_none());
        let attributes = body["object"]["Реквизиты"].as_array().unwrap();
        assert_eq!(attributes[0]["type_variants"][0]["resolution"], "source");
        let attribute = |name| attributes.iter().find(|item| item["name"] == name).unwrap();
        assert_eq!(
            attribute("Unsupported")["type_variants"][0]["reason"],
            "technical_name_unavailable"
        );
        assert_eq!(
            attribute("FutureUnknown")["type_variants"][0]["reason"],
            "unknown_technical_name"
        );
        let sections = body["object"]["ТабличныеЧасти"].as_array().unwrap();
        let rows = sections.iter().find(|item| item["name"] == "Rows").unwrap();
        assert_eq!(rows["attributes"][0]["type_variants"].as_array().unwrap().len(), 2);
        assert_eq!(
            sections.iter().find(|item| item["name"] == "EmptyRows").unwrap()["attributes"],
            serde_json::json!([])
        );
        assert_eq!(body["completeness"]["status"], "partial");
        let reasons = body["completeness"]["reasons"].as_array().unwrap();
        assert!(reasons.iter().any(|reason| {
            reason["code"] == "unresolved_type"
                && reason["path"] == "/ТабличныеЧасти/0/attributes/1/type_variants/0/technical_name"
        }));
        for reason in reasons {
            let path = reason["path"].as_str().expect("reason path is a JSON Pointer");
            if path.is_empty() {
                continue;
            }
            assert!(body["object"].pointer(path).is_some(), "unresolved {path} in {body}");
        }

        let legacy: onec_client::MetadataStructureResult = serde_json::from_str(include_str!(
            "../../../bsl-analyzer/tests/fixtures/live_metadata_legacy.json"
        ))
        .unwrap();
        let legacy = live_metadata_object_response(Some("test"), legacy, 6_000).unwrap();
        let legacy = legacy.structured_content.unwrap();
        assert_eq!(legacy["object"]["ТабличныеЧасти"][0]["attributes"], serde_json::Value::Null);
        assert_eq!(legacy["completeness"]["status"], "partial");
        let legacy_reason = legacy["completeness"]["reasons"]
            .as_array()
            .unwrap()
            .iter()
            .find(|reason| reason["code"] == "legacy_tabular_attributes")
            .expect("legacy attributes reason");
        assert_eq!(legacy_reason["code"], "legacy_tabular_attributes");
        assert_eq!(legacy_reason["path"], "/ТабличныеЧасти/0/attributes");
        assert!(legacy["object"].pointer(legacy_reason["path"].as_str().unwrap()).is_some());

        let escaping = serde_json::json!({ "a~/b": true });
        let escaped_pointer = pointer_child("", "a~/b");
        assert_eq!(escaped_pointer, "/a~0~1b");
        assert_eq!(escaping.pointer(&escaped_pointer), Some(&serde_json::Value::Bool(true)));

        let mut large = object_from_fixture();
        large.attributes = (0..100)
            .map(|index| onec_client::MetadataStructureItem {
                name: format!("Attribute{index}"),
                synonym: String::new(),
                type_name: Some("Строка".to_string()),
                type_variants: vec![onec_client::MetadataTypeVariant {
                    technical_name: Some("Строка".to_string()),
                    presentation: "Строка".to_string(),
                    resolution: "source",
                    reason: None,
                }],
            })
            .collect();
        let fitted = live_metadata_object_response(Some("test"), large, 700).unwrap();
        assert!(live_result_payload_bytes(&fitted) <= 700 * 4);
        let fitted_body = fitted.structured_content.as_ref().unwrap();
        assert_eq!(fitted_body["truncated"], true);
        assert_eq!(fitted_body["completeness"]["status"], "partial");
        assert!(fitted_body["completeness"]["reasons"]
            .as_array()
            .unwrap()
            .iter()
            .any(|reason| { reason["code"] == "output_budget" && reason["path"] == "" }));
        for reason in fitted_body["completeness"]["reasons"].as_array().unwrap() {
            if reason["code"] == "output_budget" {
                assert_eq!(reason["path"], "");
            } else {
                assert!(
                    fitted_body["object"].pointer(reason["path"].as_str().unwrap()).is_some(),
                    "unresolved path after budget truncation: {reason} in {fitted_body}"
                );
            }
        }

        let error =
            live_metadata_object_response(Some("test"), object_from_fixture(), 0).unwrap_err();
        assert_eq!(error.code.0, -32602);
        assert_eq!(error.data.as_ref().unwrap()["reason"], "budget_too_small");
        let minimum = error.data.unwrap()["minimum_output_tokens"].as_u64().unwrap() as usize;
        assert!(minimum > 0);
        assert!(live_metadata_object_response(Some("test"), object_from_fixture(), minimum).is_ok());
    }

    fn object_from_fixture() -> onec_client::MetadataStructureResult {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../bsl-analyzer/tests/fixtures/live_metadata_type_variants.json"
        ))
        .unwrap();
        serde_json::from_value(fixture["ru"].clone()).unwrap()
    }

    #[test]
    fn live_tree_budget_keeps_whole_items_and_recounts_service_truncation() {
        let result = onec_client::MetadataListResult {
            items: (0..100)
                .map(|index| onec_client::MetadataListItem {
                    name: format!("Имя{index}"),
                    full_name: format!("Справочник.Имя{index}"),
                    synonym: "Значение".repeat(20),
                })
                .collect(),
            returned: 777,
            truncated: true,
        };
        let fitted = live_metadata_tree_response(Some("test"), result, 700).unwrap();
        assert!(live_result_payload_bytes(&fitted) <= 700 * 4);
        let body = fitted.structured_content.as_ref().unwrap();
        assert_eq!(body["schema_version"], "2");
        assert_eq!(body["returned"], body["items"].as_array().unwrap().len());
        assert_eq!(body["truncated"], true);
        let error = live_metadata_tree_response(
            Some("test"),
            onec_client::MetadataListResult { items: Vec::new(), returned: 0, truncated: false },
            0,
        )
        .unwrap_err();
        assert_eq!(error.code.0, -32602);
        assert_eq!(error.data.as_ref().unwrap()["reason"], "budget_too_small");
        let minimum = error.data.unwrap()["minimum_output_tokens"].as_u64().unwrap() as usize;
        assert!(minimum > 0);
        let minimum_response = live_metadata_tree_response(
            Some("test"),
            onec_client::MetadataListResult { items: Vec::new(), returned: 0, truncated: false },
            minimum,
        )
        .unwrap();
        assert_eq!(minimum_response.structured_content.unwrap()["schema_version"], "2");
    }

    #[test]
    fn live_object_budget_never_splits_union_or_tabular_section() {
        let mut union = object_from_fixture();
        union.standard_attributes.clear();
        union.attributes.clear();
        union.dimensions.clear();
        union.resources.clear();
        union.tabular_sections.clear();
        union.attributes.push(onec_client::MetadataStructureItem {
            name: "БольшойСоставнойТип".into(),
            synonym: String::new(),
            type_name: Some("Составной".into()),
            type_variants: vec![
                onec_client::MetadataTypeVariant {
                    technical_name: Some("СправочникСсылка.А".into()),
                    presentation: "А".repeat(2_000),
                    resolution: "source",
                    reason: None,
                },
                onec_client::MetadataTypeVariant {
                    technical_name: Some("СправочникСсылка.Б".into()),
                    presentation: "Б".repeat(2_000),
                    resolution: "source",
                    reason: None,
                },
            ],
        });
        let fitted_union = live_metadata_object_response(Some("test"), union, 700).unwrap();
        assert!(live_result_payload_bytes(&fitted_union) <= 700 * 4);
        let union_body = fitted_union.structured_content.as_ref().unwrap();
        assert!(union_body["object"]["Реквизиты"].as_array().unwrap().is_empty());
        assert!(union_body["truncated"].as_bool().unwrap());
        assert_eq!(
            union_body["completeness"]["reasons"].as_array().unwrap().last().unwrap()["path"],
            ""
        );

        let mut section = object_from_fixture();
        section.standard_attributes.clear();
        section.attributes.clear();
        section.dimensions.clear();
        section.resources.clear();
        section.tabular_sections = vec![onec_client::MetadataTabularSection {
            name: "ОченьБольшаяТабличнаяЧасть".into(),
            synonym: String::new(),
            attributes: Some(vec![onec_client::MetadataStructureItem {
                name: "ОченьБольшойРеквизит".into(),
                synonym: String::new(),
                type_name: Some("Строка".into()),
                type_variants: vec![onec_client::MetadataTypeVariant {
                    technical_name: Some("Строка".into()),
                    presentation: "Строка".repeat(2_000),
                    resolution: "source",
                    reason: None,
                }],
            }]),
        }];
        let fitted_section = live_metadata_object_response(Some("test"), section, 700).unwrap();
        assert!(live_result_payload_bytes(&fitted_section) <= 700 * 4);
        let section_body = fitted_section.structured_content.as_ref().unwrap();
        assert!(section_body["object"]["ТабличныеЧасти"].as_array().unwrap().is_empty());
        assert!(section_body["truncated"].as_bool().unwrap());
        assert!(section_body["completeness"]["reasons"]
            .as_array()
            .unwrap()
            .iter()
            .all(|reason| reason["code"] == "output_budget" && reason["path"] == ""));
    }

    #[test]
    fn live_service_failure_remains_an_mcp_error() {
        let error = live_metadata_error(onec_client::Error::Status {
            status: 500,
            body: "service failed".to_string(),
        });
        assert_eq!(error.code.0, -32603);
        assert!(error.message.contains("1C returned status 500"));
    }

    #[test]
    fn get_form_structure_lists_forms_under_the_given_root() {
        // The object form directory is `<root>/<TypeDir>/<object>/Forms`. Passing the repo root
        // when the configuration is nested under `src/cf` makes every form look missing — so the
        // caller must pass the configuration root. This locks that the function resolves forms
        // relative to whatever root it is handed (the config root in production).
        let tmp = tempfile::tempdir().unwrap();
        let forms = tmp.path().join("Catalogs").join("Пользователи").join("Forms");
        std::fs::create_dir_all(forms.join("ФормаСписка")).unwrap();
        std::fs::create_dir_all(forms.join("ФормаЭлемента")).unwrap();

        let result =
            get_form_structure(Some(tmp.path()), "Catalog", Some("Пользователи"), None).unwrap();
        let text = extract_text(&result);
        assert!(text.contains("ФормаСписка"), "should list ФормаСписка: {text}");
        assert!(text.contains("ФормаЭлемента"), "should list ФормаЭлемента: {text}");

        // A wrong root (the repo root, missing the `src/cf` config segment) finds nothing.
        let repo_root = tmp.path().join("repo_root_without_config");
        std::fs::create_dir_all(&repo_root).unwrap();
        assert!(
            get_form_structure(Some(&repo_root), "Catalog", Some("Пользователи"), None).is_err(),
            "a root without the object tree must error, not silently succeed",
        );
    }

    #[test]
    fn get_form_structure_lists_common_forms_without_object_name() {
        // CommonForm is a top-level form: it lives under `CommonForms/<Form>`, has no parent
        // object, and so must resolve with object_name = None (the protocol's failing case).
        let tmp = tempfile::tempdir().unwrap();
        let common = tmp.path().join("CommonForms");
        std::fs::create_dir_all(common.join("ОтправкаSMS")).unwrap();
        std::fs::create_dir_all(common.join("Настройки")).unwrap();

        let result = get_form_structure(Some(tmp.path()), "CommonForm", None, None).unwrap();
        let text = extract_text(&result);
        assert!(text.contains("ОтправкаSMS"), "should list ОтправкаSMS: {text}");
        assert!(text.contains("Настройки"), "should list Настройки: {text}");

        // The localized type name resolves to the same place.
        assert!(get_form_structure(Some(tmp.path()), "ОбщаяФорма", None, None).is_ok());
    }

    /// A "not ready" answer must be recognizable by a field, not by matching the sentence:
    /// an unrecognized retry envelope is read as content, and an object that exists then
    /// looks absent. The sentence stays in the text block for people and for consumers that
    /// matched it before the envelope existed.
    #[test]
    fn loading_carries_a_machine_readable_envelope_beside_the_sentence() {
        let report = crate::diagnostics_state::StatusReport {
            state: "loading",
            generation: 0,
            files: None,
            unread_files: None,
            reload: "none",
            error: None,
            elapsed_ms: Some(20),
            failed_age_ms: None,
            attempts: 0,
            retry_after_ms: None,
            watch: None,
        };

        let result = loading(&report);
        let body =
            result.structured_content.as_ref().expect("loading answers with structuredContent");

        assert_eq!(body["status"], "loading");
        assert_eq!(body["state"], "loading");
        assert_eq!(body["elapsed_ms"], 20);
        assert!(body["detail"].as_str().is_some_and(|d| !d.is_empty()));
        assert!(
            extract_text(&result).starts_with("Метаданные загружаются"),
            "the human sentence stays the text block: {}",
            extract_text(&result),
        );
    }

    #[test]
    fn get_form_structure_requires_object_name_for_object_forms() {
        let tmp = tempfile::tempdir().unwrap();
        let err = get_form_structure(Some(tmp.path()), "Catalog", None, None).unwrap_err();
        assert!(
            format!("{err:?}").contains("object_name"),
            "object forms must still require object_name: {err:?}",
        );
    }

    #[test]
    fn test_metadata_tree_summary() {
        let config = fixture_config();
        let db = ide::RootDatabaseImpl::new();
        let result = get_metadata_tree(&db, &config, &[], None, None, usize::MAX).unwrap();
        let text = extract_text(&result);

        assert!(text.contains("дерево метаданных"), "should have header");
        assert!(text.contains("Всего объектов:"), "should have total");
        assert!(text.contains("Справочник"), "should list catalogs");
        assert!(text.contains("Документ"), "should list documents");
    }

    #[test]
    fn test_metadata_tree_filter_catalogs() {
        let config = fixture_config();
        let result = get_metadata_tree(
            &ide::RootDatabaseImpl::new(),
            &config,
            &[],
            Some("Справочник".into()),
            None,
            usize::MAX,
        )
        .unwrap();
        let text = extract_text(&result);

        assert!(text.contains("Справочник"), "should have category name");
        assert!(text.contains("Справочник1"), "should list Справочник1");
    }

    /// A category answers to its singular and plural, Russian and English names, and to a name
    /// over-qualified with an object; the collections without an `MdoType` of their own too.
    #[test]
    fn a_category_is_recognised_by_any_name_an_agent_uses() {
        let catalog = Some(Category::Objects(MdoType::Catalog));
        let names = ["Справочник", "справочники", "Catalog", "Catalogs", "Справочник.Пользователи"];
        for name in names {
            assert_eq!(Category::parse(name), catalog, "{name}");
        }
        let report = Some(Category::Objects(MdoType::Report));
        assert_eq!(Category::parse("Отчёты"), report);
        let register = Some(Category::Objects(MdoType::InformationRegister));
        assert_eq!(Category::parse("РегистрыСведений"), register);
        let process = Some(Category::Objects(MdoType::BusinessProcess));
        assert_eq!(Category::parse("БизнесПроцесс"), process);
        for (names, category) in [
            (["ОбщийМодуль", "ОбщиеМодули"], Category::CommonModules),
            (["ПодпискаНаСобытие", "ПодпискиНаСобытия"], Category::EventSubscriptions),
            (["ОпределяемыйТип", "ОпределяемыеТипы"], Category::DefinedTypes),
            (["РегламентноеЗадание", "РегламентныеЗадания"], Category::ScheduledJobs),
            (["Роль", "Роли"], Category::Roles),
            (["HTTPСервис", "HTTPServices"], Category::Services(ServiceKind::Http)),
        ] {
            for name in names {
                assert_eq!(Category::parse(name), Some(category), "{name}");
            }
        }
        assert_eq!(Category::parse("НесуществующаяКатегория"), None);
        assert_eq!(Category::parse(""), None);
    }

    /// `meta_type` lists a category across the configuration and its extensions, as the name
    /// search does: with the objects in extensions alone it was refused as an unknown category.
    #[test]
    fn meta_type_lists_a_category_in_every_layer() {
        let db = ide::RootDatabaseImpl::new();
        let extensions = vec![("Расширение1".to_owned(), fixture_config())];
        let empty = Configuration::new("Пустая");
        let catalogs = Some("Справочники".to_owned());
        let listed = get_metadata_tree(&db, &empty, &extensions, catalogs, None, usize::MAX);
        let listed = listed.expect("a category with objects in an extension is known");
        let text = extract_text(&listed);
        assert!(text.starts_with("# Справочник (3)\n"), "{text}");
        assert!(text.contains("\n## Расширение: Расширение1 (3)\n\n- "), "{text}");
        assert!(text.contains("\n- Справочник1"), "{text}");
        assert!(!text.contains("## Конфигурация"), "an empty layer is left out: {text}");

        let config = fixture_config();
        let catalogs = Some("Справочник".to_owned());
        let filter = Some("1".to_owned());
        let both = get_metadata_tree(&db, &config, &extensions, catalogs, filter, usize::MAX);
        let both = both.unwrap();
        let text = extract_text(&both);
        assert!(text.starts_with("# Справочник (2)\n"), "{text}");
        assert!(text.contains("## Конфигурация (1)") && text.contains("Расширение1 (1)"), "{text}");
    }

    /// A category is a category by its name, also when the configuration has none of it, by
    /// its plural, and for scheduled jobs, which the summary counts.
    #[test]
    fn known_categories_are_listed_even_when_empty() {
        let db = ide::RootDatabaseImpl::new();
        let config = fixture_config();
        for (category, heading) in [
            ("БизнесПроцесс", "# БизнесПроцесс (0)\n"),
            ("ОпределяемыеТипы", "# ОпределяемыеТипы (0)\n"),
            ("РегистрыСведений", "# РегистрСведений (1)\n"),
            ("Роли", "# Роли ("),
            ("ПодпискиНаСобытия", "# ПодпискиНаСобытия ("),
            ("РегламентноеЗадание", "# РегламентныеЗадания ("),
        ] {
            let category = Some(category.to_owned());
            let listed = get_metadata_tree(&db, &config, &[], category.clone(), None, usize::MAX);
            let listed = listed.unwrap_or_else(|error| panic!("{category:?}: {error:?}"));
            let text = extract_text(&listed);
            assert!(text.starts_with(heading), "{category:?}: {text}");
        }
        let jobs = Some("РегламентныеЗадания".to_owned());
        let listed = get_metadata_tree(&db, &config, &[], jobs, None, usize::MAX).unwrap();
        assert!(extract_text(&listed).contains("\n- РегламентноеЗадание1\n"));
    }

    /// A caller that still passes a category in `filter` gets the name search, with a line
    /// that says how to list the category.
    #[test]
    fn a_filter_that_names_a_category_points_at_meta_type() {
        let db = ide::RootDatabaseImpl::new();
        let config = fixture_config();
        let filter = Some(" Справочник ".to_owned());
        let found = get_metadata_tree(&db, &config, &[], None, filter, usize::MAX).unwrap();
        let text = extract_text(&found);
        assert!(text.contains("`meta_type: \"Справочник\"`"), "{text}");
        assert!(text.contains("- Справочник.Справочник1\n"), "the search still runs: {text}");
    }

    /// `object` opens every pair the name search prints, the collections without an object
    /// lookup of their own included.
    #[test]
    fn object_opens_common_modules_roles_defined_types_and_scheduled_jobs() {
        let mut config = fixture_config();
        config.add_defined_type(
            bsl_metadata::DefinedType::builder()
                .uuid("00000000-0000-0000-0000-0000000000d1".parse().unwrap())
                .name("ТестовыйТип")
                .underlying_type(bsl_metadata::AttributeType::String { length: Some(10) })
                .build(),
        );
        for (object_type, name) in [
            ("ОбщийМодуль", "ПервыйОбщийМодуль"),
            ("Роль", "Роль1"),
            ("ОпределяемыйТип", "ТестовыйТип"),
            ("РегламентноеЗадание", "РегламентноеЗадание1"),
        ] {
            assert!(is_resolvable_object_type(object_type), "{object_type}");
            let card = get_object_structure(&config, object_type, name);
            let card = card.unwrap_or_else(|error| panic!("{object_type}.{name}: {error:?}"));
            let text = extract_text(&card);
            assert!(text.starts_with(&format!("# {object_type}.{name}\n")), "{text}");
        }
        let missing = get_object_structure(&config, "Роль", "НетТакойРоли").unwrap_err();
        assert!(missing.message.contains("Роль.НетТакойРоли"), "{}", missing.message);
    }

    #[test]
    fn test_metadata_tree_filter_plural_and_dotted_resolve() {
        let config = fixture_config();
        for input in ["Catalogs", "Справочники", "Справочник.Справочник1"]
        {
            let result = get_metadata_tree(
                &ide::RootDatabaseImpl::new(),
                &config,
                &[],
                Some(input.into()),
                None,
                usize::MAX,
            )
            .unwrap();
            let text = extract_text(&result);
            assert!(
                text.contains("Справочник1"),
                "filter `{input}` should list Справочник1: {text}"
            );
        }
    }

    #[test]
    fn test_metadata_tree_filter_common_modules() {
        let config = fixture_config();
        let result = get_metadata_tree(
            &ide::RootDatabaseImpl::new(),
            &config,
            &[],
            Some("ОбщиеМодули".into()),
            None,
            usize::MAX,
        )
        .unwrap();
        let text = extract_text(&result);

        assert!(text.contains("ОбщиеМодули"), "should have category name");
    }

    #[test]
    fn test_metadata_tree_filter_invalid() {
        let config = fixture_config();
        let result = get_metadata_tree(
            &ide::RootDatabaseImpl::new(),
            &config,
            &[],
            Some("НесуществующаяКатегория".into()),
            None,
            usize::MAX,
        );

        let error = result.expect_err("an unknown category is refused");
        assert!(error.message.contains("`meta_type`"), "{}", error.message);
        assert!(!error.message.contains("get_metadata_tree"), "{}", error.message);
    }

    /// `filter` is what its schema says it is: a case-insensitive substring of object names.
    /// It was read as a category, so a name fragment such as `Заказ` came back as an
    /// unknown category, with an error that named a tool the server does not have. The search
    /// covers every collection of the configuration and of each extension, one
    /// `Категория.Имя` line per object, which is the pair `object` takes.
    #[test]
    fn tree_filter_finds_objects_by_a_name_substring_in_every_layer() {
        let config = fixture_config();
        let extensions = vec![("Расширение1".to_owned(), fixture_config())];
        let db = ide::RootDatabaseImpl::new();

        let filter = Some("СПРАВОЧНИК".to_owned());
        let found = get_metadata_tree(&db, &config, &extensions, None, filter, usize::MAX);
        let found = found.expect("a name substring is not a category");
        let text = extract_text(&found);
        assert!(text.contains("- Справочник.Справочник1\n"), "{text}");
        assert!(text.contains("- Справочник.СправочникСМенеджером\n"), "{text}");
        assert!(text.contains("- ПодпискаНаСобытие.ПриЗаписиСправочника\n"), "{text}");
        assert!(!text.contains("рдт_Рецептура"), "{text}");
        assert!(text.contains("## Конфигурация (3)"), "{text}");
        assert!(text.contains("## Расширение: Расширение1 (3)"), "{text}");
        assert!(text.starts_with("# Объекты, в имени которых есть «СПРАВОЧНИК» (6)"), "{text}");

        let filter = Some("рдт_".to_owned());
        let found = get_metadata_tree(&db, &config, &[], None, filter, usize::MAX).unwrap();
        assert!(extract_text(&found).contains("- Справочник.рдт_Рецептура\n"));

        // Collections outside the object table are searched too.
        let filter = Some("ЗаданиеНесуществующийМетод".to_owned());
        let found = get_metadata_tree(&db, &config, &[], None, filter, usize::MAX).unwrap();
        let text = extract_text(&found);
        assert!(
            text.contains("- РегламентноеЗадание.РегламентноеЗаданиеНесуществующийМетод\n"),
            "{text}"
        );

        let filter = Some("НетТакогоИмени".to_owned());
        let nothing = get_metadata_tree(&db, &config, &[], None, filter, usize::MAX);
        let nothing = nothing.expect("no match is an empty answer, not an error");
        assert!(extract_text(&nothing).contains("(0)"), "{}", extract_text(&nothing));
        assert!(extract_text(&nothing).contains("`meta_type`"), "{}", extract_text(&nothing));
    }

    /// The category has its own parameter, and `filter` narrows it. A category whose objects
    /// all miss the filter is listed empty rather than refused as unknown.
    #[test]
    fn tree_filter_narrows_the_meta_type_category() {
        let config = fixture_config();
        let db = ide::RootDatabaseImpl::new();

        let category = Some("Справочники".to_owned());
        let filter = Some("менеджер".to_owned());
        let narrowed = get_metadata_tree(&db, &config, &[], category, filter, usize::MAX).unwrap();
        let text = extract_text(&narrowed);
        assert!(text.starts_with("# Справочник (1)\n"), "{text}");
        assert!(text.contains("- СправочникСМенеджером"), "{text}");
        assert!(!text.contains("- Справочник1"), "{text}");

        let category = Some("Справочник".to_owned());
        let filter = Some("НетТакогоИмени".to_owned());
        let empty = get_metadata_tree(&db, &config, &[], category, filter, usize::MAX).unwrap();
        assert!(extract_text(&empty).starts_with("# Справочник (0)\n"), "{}", extract_text(&empty));
    }

    #[test]
    fn test_object_structure_catalog() {
        let config = fixture_config();
        let result = get_object_structure(&config, "Catalog", "Справочник1").unwrap();
        let text = extract_text(&result);

        assert!(text.contains("Справочник.Справочник1"), "should have object header");
    }

    #[test]
    fn test_object_structure_event_subscription() {
        // Regression: `ПодпискаНаСобытие` used to fail to parse as an MDO type ("Unknown
        // MDO type"); it now resolves and reports its source/event/handler.
        let config = fixture_config();
        let result =
            get_object_structure(&config, "ПодпискаНаСобытие", "ПриЗаписиСправочника").unwrap();
        let text = extract_text(&result);
        assert!(text.contains("ПодпискаНаСобытие.ПриЗаписиСправочника"), "header");
        assert!(text.contains("Событие: OnWrite"), "event");
        assert!(
            text.contains("Обработчик: CommonModule.ОбщийПодпискиНаСобытия.ПриЗаписиСправочника"),
            "handler string"
        );
        assert!(text.contains("Метод: ПриЗаписиСправочника"), "parsed handler method");
    }

    #[test]
    fn test_object_structure_event_subscription_not_found_is_graceful() {
        let config = fixture_config();
        // A missing subscription is an in-band invalid-params error, not a parse crash.
        assert!(get_object_structure(&config, "ПодпискаНаСобытие", "НетТакой").is_err());
    }

    #[test]
    fn test_object_structure_not_found() {
        let config = fixture_config();
        let result = get_object_structure(&config, "Catalog", "НесуществующийОбъект");

        assert!(result.is_err(), "should return error for missing object");
    }

    #[test]
    fn test_object_structure_invalid_type() {
        let config = fixture_config();
        let result = get_object_structure(&config, "InvalidType", "Test");

        assert!(result.is_err(), "should return error for invalid type");
    }

    #[test]
    fn test_configuration_info() {
        let config = fixture_config();
        let result = get_configuration_info(&config, &[]).unwrap();
        let text = extract_text(&result);

        assert!(text.contains("# Конфигурация:"), "should have config header");
        assert!(text.contains("UUID:"), "should have UUID");
        assert!(text.contains("Общие модули:"), "should have common modules count");
        assert!(text.contains("Регистры:"), "should have registers count");
    }

    #[test]
    fn test_form_structure_list_forms() {
        let fixture_root =
            concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-metadata/fixtures/designer");
        let root = std::path::Path::new(fixture_root);
        let result = get_form_structure(Some(root), "Document", Some("Документ1"), None).unwrap();
        let text = extract_text(&result);

        assert!(text.contains("Формы Document.Документ1"), "should have forms header");
        assert!(text.contains("ФормаДокумента"), "should list document form");
        assert!(text.contains("ФормаСписка"), "should list list form");
    }

    #[test]
    fn test_form_structure_specific_form() {
        let fixture_root =
            concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-metadata/fixtures/designer");
        let root = std::path::Path::new(fixture_root);
        let result =
            get_form_structure(Some(root), "Document", Some("Документ1"), Some("ФормаДокумента"))
                .unwrap();
        let text = extract_text(&result);

        assert!(text.contains("# Форма:"), "should have form header");
        // The heading carries a name (parsed, or the requested form name as fallback when
        // Ext/Form.xml omits it), never a blank title.
        assert!(!text.contains("# Форма: \n"), "heading must not be blank: {text}");
        // A nil UUID (the common case — Form.xml carries no UUID) is labelled, never printed as
        // a misleading all-zero UUID.
        assert!(
            !text.contains("00000000-0000-0000-0000-000000000000"),
            "nil UUID must be labelled, not printed as zeros: {text}",
        );
    }

    #[test]
    fn test_form_structure_no_workspace() {
        let result = get_form_structure(None, "Catalog", Some("Test"), None);
        assert!(result.is_err(), "should fail without workspace root");
    }

    #[test]
    fn test_form_structure_form_not_found() {
        let fixture_root =
            concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-metadata/fixtures/designer");
        let root = std::path::Path::new(fixture_root);
        let result = get_form_structure(
            Some(root),
            "Document",
            Some("Документ1"),
            Some("НесуществующаяФорма"),
        );

        assert!(result.is_err(), "should fail for missing form");
    }

    // ===== Wave 2d: explicit service enumerate/object/config-info =====
    //
    // These pin the contract that the MCP metadata tools enumerate HTTP/Web/Integration
    // services as explicit category listings and produce structured object dumps, instead
    // of forcing the agent through the hot typed config resolver. They RED-fail until
    // `format_filtered_tree` grows HTTP/Web/Integration-service branches and
    // `get_object_structure` recognises the service MDO types.

    #[test]
    fn metadata_tree_filter_lists_http_services_explicitly() {
        let config = fixture_config();
        let result = get_metadata_tree(
            &ide::RootDatabaseImpl::new(),
            &config,
            &[],
            Some("HTTPСервис".into()),
            None,
            usize::MAX,
        )
        .unwrap();
        let text = extract_text(&result);

        assert!(text.contains("HTTPСервис1"), "filter must enumerate http services: {text}");
        assert!(
            text.contains("HTTP") && text.contains("Сервис"),
            "filter must label the http-service category explicitly: {text}"
        );
    }

    #[test]
    fn metadata_tree_filter_lists_web_services_explicitly() {
        let config = fixture_config();
        let result = get_metadata_tree(
            &ide::RootDatabaseImpl::new(),
            &config,
            &[],
            Some("WebСервис".into()),
            None,
            usize::MAX,
        )
        .unwrap();
        let text = extract_text(&result);

        assert!(text.contains("WebСервис1"), "filter must enumerate web services: {text}");
    }

    #[test]
    fn metadata_tree_filter_lists_integration_services_explicitly() {
        // IntegrationService is deliberately absent from `MetadataReferenceKind` (no
        // `Метаданные.СервисыИнтеграции` plural), but the MCP enumeration surface must
        // still list integration services as a discrete category for agent discovery.
        let config = fixture_config();
        let result = get_metadata_tree(
            &ide::RootDatabaseImpl::new(),
            &config,
            &[],
            Some("СервисИнтеграции".into()),
            None,
            usize::MAX,
        )
        .unwrap();
        let text = extract_text(&result);

        assert!(
            text.contains("ОбменСообщениями"),
            "filter must enumerate integration services: {text}"
        );
    }

    #[test]
    fn object_structure_dumps_http_service_explicitly() {
        let config = fixture_config();
        let result = get_object_structure(&config, "HTTPService", "HTTPСервис1").unwrap();
        let text = extract_text(&result);

        assert!(
            text.contains("HTTPService.HTTPСервис1") || text.contains("HTTPСервис1"),
            "object structure must name the http service: {text}"
        );
        assert!(
            text.contains("URLTemplate") || text.contains("Метод") || text.contains("RootURL"),
            "object structure must dump http-service shape (templates/methods/root): {text}"
        );
    }

    #[test]
    fn object_structure_dumps_web_service_explicitly() {
        let config = fixture_config();
        let result = get_object_structure(&config, "WebService", "WebСервис1").unwrap();
        let text = extract_text(&result);

        assert!(
            text.contains("WebService.WebСервис1") || text.contains("WebСервис1"),
            "object structure must name the web service: {text}"
        );
        assert!(
            text.contains("Операция") || text.contains("Namespace"),
            "object structure must dump web-service shape (operations/namespace): {text}"
        );
    }

    #[test]
    fn object_structure_dumps_integration_service_explicitly() {
        let config = fixture_config();
        let result =
            get_object_structure(&config, "IntegrationService", "ОбменСообщениями").unwrap();
        let text = extract_text(&result);

        assert!(
            text.contains("IntegrationService.ОбменСообщениями")
                || text.contains("ОбменСообщениями"),
            "object structure must name the integration service: {text}"
        );
        assert!(
            text.contains("Канал") || text.contains("Channel"),
            "object structure must dump integration-service channels: {text}"
        );
    }

    #[test]
    fn configuration_info_reports_http_and_web_service_counts_explicitly() {
        let config = fixture_config();
        let result = get_configuration_info(&config, &[]).unwrap();
        let text = extract_text(&result);

        assert!(
            text.contains("HTTP-сервисы:") && !text.contains("HTTP-сервисы: 0"),
            "configuration info must report non-zero http-service count: {text}"
        );
        assert!(
            text.contains("Web-сервисы:") && !text.contains("Web-сервисы: 0"),
            "configuration info must report non-zero web-service count: {text}"
        );
    }

    /// The force-rescan retry gate predicate must accept every type `object_from_db` can
    /// dispatch — including HTTP/Web/Integration services, which are NOT `MdoType` variants
    /// (they route through `parse_service_kind`). Gating on `MdoType::from_str` alone (the
    /// bug) would classify a service miss as non-retryable, so a just-added service would
    /// never force a re-scan.
    #[test]
    fn is_resolvable_object_type_covers_services_not_in_mdotype() {
        for t in
            ["HTTPService", "WebService", "IntegrationService", "httpсервис", "сервисинтеграции"]
        {
            assert!(
                t.parse::<MdoType>().is_err(),
                "{t} is deliberately not an MdoType — this is what the old gate missed",
            );
            assert!(
                is_resolvable_object_type(t),
                "{t} must be resolvable (dispatched via parse_service_kind)",
            );
        }
        for t in ["Catalog", "InformationRegister", "ПодпискаНаСобытие", "Справочник"]
        {
            assert!(is_resolvable_object_type(t), "{t} (an MdoType) must stay resolvable");
        }
        assert!(
            !is_resolvable_object_type("НеизвестныйТип"),
            "a genuinely unknown type must not be classified resolvable (no wasted force-scan)",
        );
    }
}

#[cfg(test)]
mod form_probe_case_tests {
    #[test]
    fn a_case_variant_form_xml_is_found_by_the_tool_probe() {
        let dir = tempfile::tempdir().unwrap();
        let container = dir.path().join("Forms");
        std::fs::create_dir_all(container.join("Ф/EXT")).unwrap();
        std::fs::write(container.join("Ф/EXT/FORM.XML"), "<Form/>").unwrap();
        let result = super::forms_in_container(&container, Some("Ф"), "Тест");
        assert!(result.is_ok(), "форма с EXT/FORM.XML читается: {result:?}");
    }
}
