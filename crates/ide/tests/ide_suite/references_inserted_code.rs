//! References in code an extension inserts with `#Вставка` into a method it changes with
//! `&ИзменениеИКонтроль`. The extension's own parse keeps that code as raw tokens, so a walk
//! of that parse alone finds no call there.

use ide::{
    find_references_by_name, ReferenceAnchor, ReferenceArea, ReferenceKind, ReferencesOutcome,
    ReferencesRequest,
};
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::metadata::WorkspaceConfigsSnapshot;
use ide_db::RootDatabaseImpl;
use vfs::{FileId, FileSet, VfsPath};

const BASE: &str = "Процедура Ф() Экспорт
    А = 1;
КонецПроцедуры
";

const LIBRARY: &str = "Процедура Метод() Экспорт
КонецПроцедуры
";

const LIBRARY_XML: &str = r#"<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses">
    <CommonModule uuid="00000000-0000-0000-0000-000000000002">
        <Properties>
            <Name>D_Модуль</Name>
            <Global>false</Global>
            <Server>true</Server>
        </Properties>
    </CommonModule>
</MetaDataObject>
"#;

const EXTENSION: &str = r#"&ИзменениеИКонтроль("Ф")
Процедура E_Ф()
    А = 1;
#Вставка
    D_Модуль.Метод();
#КонецВставки
КонецПроцедуры

Процедура E_Своя()
    D_Модуль.Метод();
КонецПроцедуры
"#;

/// The base module `М`, the extension `D` with the common module `D_Модуль`, and the
/// extension `E` that depends on `D` and changes `М`. Files: 0 the base `М`, 1 `E`'s `М`,
/// 2 `D_Модуль`. The directory must outlive the database.
fn setup() -> (tempfile::TempDir, RootDatabaseImpl) {
    let temp = tempfile::tempdir().unwrap();
    let roots = ["src/cf", "src/cfe/D", "src/cfe/E"].map(|dir| temp.path().join(dir));
    for root in &roots {
        std::fs::create_dir_all(root.join("CommonModules")).unwrap();
        std::fs::write(root.join("Configuration.xml"), "<Configuration/>").unwrap();
    }
    std::fs::write(roots[1].join("CommonModules/D_Модуль.xml"), LIBRARY_XML).unwrap();

    let mut snapshot = WorkspaceConfigsSnapshot::from_paths(vec![
        (None, roots[0].clone()),
        (Some("D".to_string()), roots[1].clone()),
        (Some("E".to_string()), roots[2].clone()),
    ]);
    let info_base = Some(bsl_metadata::ExtensionScope::InfoBase);
    snapshot.extension_scopes = vec![None, info_base, info_base];
    snapshot.closures[2] = vec![1];
    let mut db = RootDatabaseImpl::new();
    db.set_workspace_configs_snapshot(snapshot);

    let files = [
        (roots[0].join("CommonModules/М/Ext/Module.bsl"), BASE),
        (roots[2].join("CommonModules/М/Ext/Module.bsl"), EXTENSION),
        (roots[1].join("CommonModules/D_Модуль/Ext/Module.bsl"), LIBRARY),
    ];
    let mut file_set = FileSet::default();
    for (index, (path, _)) in files.iter().enumerate() {
        file_set.insert(FileId(index as u32), VfsPath::new(path.to_string_lossy().as_ref()));
    }
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    for (index, (_, text)) in files.iter().enumerate() {
        let file = FileId(index as u32);
        db.set_file_source_root(file, SourceRootId(0));
        db.set_file_text(file, text);
    }
    (temp, db)
}

/// A call in inserted code is a reference: it is read in the assembled module, with the
/// visibility of the extension that runs it, and classified there as a call. The call in the
/// extension's own method is the control the walk of the extension's parse answers alone.
#[test]
fn inserted_call_is_a_reference_of_the_dependency_method() {
    let (_temp, db) = setup();
    let request = ReferencesRequest {
        anchor: ReferenceAnchor::Position { file_id: FileId(2), line: 0, column: 10 },
        anchor_files: None,
        area: ReferenceArea::default(),
        kinds: Some(vec![ReferenceKind::Call]),
        include_declaration: false,
        max_files: 100,
    };

    let result = find_references_by_name(&db, &request, &[]);

    assert_eq!(result.outcome, ReferencesOutcome::Resolved);
    assert!(result.hits.iter().all(|hit| hit.file_id == FileId(1)), "{:?}", result.hits);
    let call_offset = |at: usize| (at + "D_Модуль.".len()) as u32;
    let inserted = call_offset(EXTENSION.find("D_Модуль.Метод").unwrap());
    let own = call_offset(EXTENSION.rfind("D_Модуль.Метод").unwrap());
    let mut offsets: Vec<u32> =
        result.hits.iter().map(|hit| u32::from(hit.range.start())).collect();
    offsets.sort_unstable();
    assert_eq!(offsets, vec![inserted, own], "{:?}", result.hits);
}
