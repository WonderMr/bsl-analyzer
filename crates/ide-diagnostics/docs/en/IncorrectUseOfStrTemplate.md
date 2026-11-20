# Incorrect use of "StrTemplate" (IncorrectUseOfStrTemplate)

<!-- Блоки выше заполняются автоматически, не трогать -->
## Description
This diagnostic validates the API contract of `StrTemplate` / `СтрШаблон`.

The current implementation checks several common problems:

- the number of passed arguments does not match the placeholders in the template;
- invalid placeholder numbers such as `%0` or `%11` are used;
- `%%` escapes are handled incorrectly;
- a template is passed indirectly through a variable that resolves to an invalid
  literal;
- misplaced parentheses around `NStr(...)` make the template expression wrong.

A multi-line template literal (continuation lines starting with `|`) is checked as a
whole. A template added up from string literals (`"Document No. %1" + " of %2"`,
including one in parentheses or with an `NStr(...)` among the parts) is checked as
one string. The parts `Chars.LF`, `Chars.CR`, `Chars.Tab`, `Chars.VTab`, `Chars.FF` and
`Chars.NBSp` are read as text without placeholders.

Each branch of `?(Condition, "...", "...")` is checked as a template of its own. For a
multilingual `NStr("ru = '...'; en = '...'")` the translation into each language is
checked separately, and a translation whose placeholders do not match the arguments is
reported. When the second argument of `NStr` is a language code given as a string, only
the translation into that language is checked. The translations of several `NStr` calls in
one template are joined only within one language: the user's language selects all of them.

A template held in a local variable is checked when every assignment that reaches the
call builds it from the same kinds of parts and all of them give the same text:
`Template = "Document %1" + " of %2"; Text = StrTemplate(Template, Number)` is reported.

When any part of the template is unknown (a method parameter, a function call, an object
attribute, another property of `Chars`), the diagnostic stays silent: placeholders may
come from that part.

It is also important to remember:

- `StrTemplate` supports placeholders from `%1` to `%10`;
- if a digit must follow a substituted value immediately, the placeholder should
  use parentheses, for example `"%(1)45"`.

## Examples
<!-- В данном разделе приводятся примеры, на которые диагностика срабатывает, а также можно привести пример, как можно исправить ситуацию -->

Invalid: the number of arguments does not match the placeholders

```bsl
StrTemplate("Name (version %1)");
StrTemplate("%1 (version %2)", Name);
```

Invalid: `NStr` parentheses are misplaced

```bsl
StrTemplate(NStr("en='Name (version %1)'", Version()));
```

Invalid: one branch or one translation does not match the arguments

```bsl
StrTemplate(?(HasDate, "Document %1 of %2", "Document %1"), Number);
StrTemplate(NStr("ru = 'Документ %1'; en = 'Document'"), Number);
```

Correct:

```bsl
StrTemplate(NStr("en='Name (version %1)'"), Version());
StrTemplate("Name %(1)2", Name);
StrTemplate("Kind: %1" + Chars.LF + "Code: %2", Kind, Code);
StrTemplate(?(HasDate, "Document %1 of %2", "Document %1 (%2)"), Number, Date);
```

## Sources

This diagnostic has no direct normative 1C standard source.

Related public context:

* [ITS / v8std #std763: localization requirements for formatting strings](https://its.1c.ru/db/v8std#content:763:hdoc)
