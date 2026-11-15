//! Language registry. Adding a language = adding one entry to `LANGUAGES`
//! (plus its icon in `assets/icons/files`, and its grammar in `register_grammars`
//! if gpui-component doesn't bundle it).

use std::path::Path;

use gpui_component::highlighter::{LanguageConfig, LanguageRegistry};

use crate::lsp::{self, Server};

/// Space-separated words, most common first: the rank is the completion prior.
pub type Words = &'static [&'static str];

/// What a declaration keyword introduces (`fn` → function, `let` → variable…).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Decl {
    Variable,
    Function,
    Class,
    Type,
    /// Every name after it on the same line (`import a, b`, `use x::{y, z}`).
    Module,
}

#[derive(Clone, Copy)]
pub struct Language {
    pub name: &'static str,
    /// tree-sitter grammar name understood by gpui-component's highlighter.
    pub highlight: &'static str,
    pub extensions: &'static [&'static str],
    /// Characters that belong to identifiers besides alphanumerics and `_`.
    pub word_chars: &'static str,
    /// Code (not markup/data): detect declarations and `x = …` assignments.
    pub code: bool,
    // Lexing, so that words inside comments and strings are not taken for code.
    pub line_comments: &'static [&'static str],
    pub block_comment: Option<(&'static str, &'static str)>,
    pub quotes: &'static str,
    /// Keywords that start a statement, proposed first at the start of a line.
    pub statements: Words,
    /// Known names usable in an expression (values, globals, builtins…).
    pub globals: Words,
    /// Known members, proposed after `.`, `->` or `::`.
    pub members: Words,
    /// Keyword → what the following name is.
    pub declarers: &'static [(&'static str, Decl)],
    /// Default run command. `{file}`: quoted file path, `{out}`: temp binary path.
    /// Overridable per project in `akolade.json`.
    pub run: Option<&'static str>,
    pub color: u32,
    pub icon: &'static str,
    /// Language server for semantic completion (`obj.` → its real members).
    pub server: Option<Server>,
}

const BASE: Language = Language {
    name: "Texte",
    highlight: "text",
    extensions: &[],
    word_chars: "",
    code: false,
    line_comments: &[],
    block_comment: None,
    quotes: "",
    statements: &[],
    globals: &[],
    members: &[],
    declarers: &[],
    run: None,
    color: 0x7d8590,
    icon: "icons/files/text.svg",
    server: None,
};

const fn npm(program: &'static str, language_id: &'static str, install: &'static str) -> Option<Server> {
    Some(Server { program, args: &["--stdio"], language_id, install: Some(install), init: None })
}

const fn native(program: &'static str, language_id: &'static str, install: Option<&'static str>) -> Option<Server> {
    Some(Server { program, args: &[], language_id, install, init: None })
}

const fn typescript(language_id: &'static str) -> Option<Server> {
    Some(Server { program: "typescript-language-server", args: &["--stdio"], language_id, install: Some(TS_SERVER), init: Some(lsp::tsserver_options) })
}

// TypeScript 7 (native) has no tsserver.js, which typescript-language-server needs.
const TS_SERVER: &str = "npm install --prefix {dir} typescript-language-server typescript@5";
const WEB_SERVERS: &str = "npm install --prefix {dir} vscode-langservers-extracted";

/// `//` and `/* */` comments, "strings" ('c' chars are left to the code).
const C_LIKE: Language = Language {
    code: true,
    line_comments: &["//"],
    block_comment: Some(("/*", "*/")),
    quotes: "\"",
    ..BASE
};

const HASH_COMMENTS: Language = Language { code: true, line_comments: &["#"], quotes: "\"'", ..BASE };

#[cfg(target_os = "macos")]
const OPEN: &str = "open {file}";
#[cfg(not(target_os = "macos"))]
const OPEN: &str = "xdg-open {file}";

// ---------- word lists (most common first) ----------

const JS_STMT: &str = "const return if import export function let else await for async throw try catch class default case break switch while var continue finally delete do yield debugger";
const JS_EXPR: &str = "this true false null new undefined await function async typeof console Object Array JSON Promise Math document window Error String Number Date Map Set require module exports process fetch setTimeout Boolean RegExp Symbol in of instanceof void super globalThis setInterval clearTimeout structuredClone";
const JS_MEMBERS: &str = "length map push log then forEach filter join error includes split slice keys find toString replace indexOf reduce trim some catch entries values addEventListener querySelector stringify parse warn info every findIndex pop shift splice finally resolve reject all textContent innerHTML classList preventDefault getElementById createElement appendChild querySelectorAll removeEventListener";
const TS_STMT: &str = "type interface private public readonly static enum declare abstract protected namespace";
const TS_EXPR: &str = "string number boolean as any extends Record unknown keyof Partial never implements Omit Pick Readonly satisfies";
const JSX: &str = "className onClick onChange useState useEffect useRef useMemo useCallback React props children key";
const HTML: &str = "div span a p button input li img ul class id href src alt type name value html head body title meta link script style main header footer nav section article aside h1 h2 h3 h4 ol table thead tbody tr td th form label select option textarea canvas svg video audio source template slot placeholder charset content rel lang width height onclick disabled checked role defer async data- aria-label";
const CSS: &str = "color display margin padding width height background font-size flex position border none center top left right bottom background-color font-weight border-radius justify-content align-items grid absolute relative fixed sticky block inline-block min-width max-width min-height max-height box-sizing border-box font-family line-height text-align flex-direction gap grid-template-columns overflow hidden auto opacity z-index transition transform animation cursor pointer box-shadow outline content important inherit transparent space-between column row wrap var calc rgba media hover focus root";

const JS_DECL: &[(&str, Decl)] = &[
    ("const", Decl::Variable), ("let", Decl::Variable), ("var", Decl::Variable),
    ("function", Decl::Function), ("class", Decl::Class), ("interface", Decl::Type),
    ("type", Decl::Type), ("enum", Decl::Type), ("import", Decl::Module),
];

pub static LANGUAGES: &[Language] = &[
    // ---------- web ----------
    Language {
        name: "JavaScript", highlight: "javascript", extensions: &["js", "mjs", "cjs"], word_chars: "$", quotes: "\"'`",
        statements: &[JS_STMT], globals: &[JS_EXPR], members: &[JS_MEMBERS], declarers: JS_DECL,
        run: Some("node {file}"), color: 0xf1d756, icon: "icons/files/js.svg", server: typescript("javascript"), ..C_LIKE
    },
    // Node >= 22.18 runs .ts natively (type stripping).
    Language {
        name: "TypeScript", highlight: "typescript", extensions: &["ts", "mts", "cts"], word_chars: "$", quotes: "\"'`",
        statements: &[JS_STMT, TS_STMT], globals: &[JS_EXPR, TS_EXPR], members: &[JS_MEMBERS], declarers: JS_DECL,
        run: Some("node {file}"), color: 0x4d9df0, icon: "icons/files/ts.svg", server: typescript("typescript"), ..C_LIKE
    },
    Language {
        name: "JSX", highlight: "javascript", extensions: &["jsx"], word_chars: "$", quotes: "\"'`",
        statements: &[JS_STMT], globals: &[JS_EXPR, JSX, HTML], members: &[JS_MEMBERS], declarers: JS_DECL,
        run: Some("npx tsx {file}"), color: 0x61dafb, icon: "icons/files/react.svg", server: typescript("javascriptreact"), ..C_LIKE
    },
    Language {
        name: "TSX", highlight: "tsx", extensions: &["tsx"], word_chars: "$", quotes: "\"'`",
        statements: &[JS_STMT, TS_STMT], globals: &[JS_EXPR, TS_EXPR, JSX, HTML], members: &[JS_MEMBERS], declarers: JS_DECL,
        run: Some("npx tsx {file}"), color: 0x61dafb, icon: "icons/files/react-ts.svg", server: typescript("typescriptreact"), ..C_LIKE
    },
    // Attribute values are kept (class names are useful completions).
    Language {
        name: "HTML", highlight: "html", extensions: &["html", "htm"], word_chars: "-", block_comment: Some(("<!--", "-->")),
        globals: &[HTML, CSS, JS_STMT, JS_EXPR], members: &[JS_MEMBERS],
        run: Some(OPEN), color: 0xf0764d, icon: "icons/files/html.svg", server: npm("vscode-html-language-server", "html", WEB_SERVERS), ..BASE
    },
    Language {
        name: "CSS", highlight: "css", extensions: &["css"], word_chars: "-", block_comment: Some(("/*", "*/")), quotes: "\"'",
        globals: &[CSS], color: 0xa77bf3, icon: "icons/files/css.svg", server: npm("vscode-css-language-server", "css", WEB_SERVERS), ..BASE
    },
    Language {
        name: "SCSS", highlight: "css", extensions: &["scss", "sass"], word_chars: "-$", line_comments: &["//"], block_comment: Some(("/*", "*/")), quotes: "\"'",
        globals: &[CSS, "mixin include extend import use if else each"], color: 0xcd6799, icon: "icons/files/scss.svg", server: npm("vscode-css-language-server", "scss", WEB_SERVERS), ..BASE
    },
    Language {
        name: "JSON", highlight: "json", extensions: &["json", "jsonc"], globals: &["true false null"],
        color: 0xe5c07b, icon: "icons/files/json.svg", server: npm("vscode-json-language-server", "json", WEB_SERVERS), ..BASE
    },
    // ---------- popular languages ----------
    Language {
        name: "Python", highlight: "python", extensions: &["py", "pyw"], quotes: "\"'",
        statements: &["def return if import from for in else elif class try except with while raise pass break continue lambda yield assert global nonlocal del async await finally match case"],
        globals: &["self None True False print len range str int dict list set tuple isinstance enumerate zip open super sorted map filter sum min max any all abs type float bool object Exception ValueError TypeError KeyError __name__ __init__ not and or is in"],
        members: &["append items get keys values join split strip format replace startswith endswith lower upper extend pop update read write close sort index count add remove insert copy encode decode"],
        declarers: &[("def", Decl::Function), ("class", Decl::Class), ("import", Decl::Module)],
        run: Some("python3 {file}"), color: 0x4b8bbe, icon: "icons/files/python.svg", server: npm("basedpyright-langserver", "python", "npm install --prefix {dir} basedpyright"), ..HASH_COMMENTS
    },
    // Java 11+ runs a single source file directly.
    Language {
        name: "Java", highlight: "java", extensions: &["java"],
        statements: &["public private return if import class static final void new else for this throw try catch protected interface extends implements package while case break switch default continue finally enum abstract synchronized do assert var record"],
        globals: &["String int boolean null true false this new System Override List Map ArrayList HashMap Integer Object Exception long double char byte float Arrays Collections Optional Stream"],
        members: &["out println print length size get add put equals toString hashCode isEmpty contains stream map filter collect forEach of format append trim split substring charAt valueOf"],
        declarers: &[("class", Decl::Class), ("interface", Decl::Type), ("enum", Decl::Type), ("record", Decl::Class), ("import", Decl::Module)],
        run: Some("java {file}"), color: 0xed8b00, icon: "icons/files/java.svg", server: native("jdtls", "java", Some("brew install jdtls")), ..C_LIKE
    },
    Language {
        name: "C", highlight: "c", extensions: &["c", "h"],
        statements: &["return if int void char else for struct static const while include define unsigned break case sizeof switch typedef enum long double float default continue do goto extern union volatile signed short"],
        globals: &["NULL printf malloc free size_t strlen memcpy memset scanf fprintf stderr stdout fopen fclose exit uint8_t uint32_t int64_t bool true false EOF"],
        declarers: &[("struct", Decl::Type), ("enum", Decl::Type), ("union", Decl::Type), ("define", Decl::Variable)],
        run: Some("cc {file} -o {out} && {out}"), color: 0x7986cb, icon: "icons/files/c.svg", server: native("clangd", "c", Some("xcode-select --install")), ..C_LIKE
    },
    Language {
        name: "C++", highlight: "cpp", extensions: &["cpp", "cc", "cxx", "hpp", "hh", "hxx"],
        statements: &["return if int void auto const else for class struct public private include namespace using template typename static virtual override while break case switch protected new delete try catch throw enum default continue do inline constexpr friend operator typedef"],
        globals: &["std cout cin endl string vector map unordered_map shared_ptr unique_ptr make_shared make_unique nullptr true false size_t this printf move"],
        members: &["push_back size begin end empty clear insert erase find at front back emplace_back first second c_str length substr get reset"],
        declarers: &[("class", Decl::Class), ("struct", Decl::Type), ("enum", Decl::Type), ("namespace", Decl::Module), ("using", Decl::Type), ("define", Decl::Variable)],
        run: Some("c++ -std=c++20 {file} -o {out} && {out}"), color: 0x4c8fd6, icon: "icons/files/cpp.svg", server: native("clangd", "cpp", Some("xcode-select --install")), ..C_LIKE
    },
    Language {
        name: "C#", highlight: "csharp", extensions: &["cs"],
        statements: &["public using return if private var new class static void namespace else foreach in this for async await readonly protected override get set try catch throw interface while switch case break internal virtual abstract sealed const enum struct record"],
        globals: &["Console string int bool var null true false List Dictionary Task Exception Math DateTime Guid object decimal double IEnumerable nameof typeof"],
        members: &["WriteLine Write ToString Count Add Where Select ToList FirstOrDefault Any Length Contains Remove Clear Equals GetType Format Split Trim Substring Result"],
        declarers: &[("class", Decl::Class), ("interface", Decl::Type), ("enum", Decl::Type), ("struct", Decl::Type), ("record", Decl::Class), ("namespace", Decl::Module), ("var", Decl::Variable)],
        run: Some("dotnet run"), color: 0x9b4f96, icon: "icons/files/csharp.svg", server: native("csharp-ls", "csharp", Some("dotnet tool install --global csharp-ls")), ..C_LIKE
    },
    Language {
        name: "Go", highlight: "go", extensions: &["go"], quotes: "\"`",
        statements: &["func return if err package import var type for range else struct interface const defer go switch case break continue select default map chan fallthrough goto"],
        globals: &["err nil fmt string int error bool true false make len append byte rune float64 int64 uint context errors strings strconv time os io http json log new panic recover cap copy delete close"],
        members: &["Println Printf Sprintf Errorf Error String New Get Set Close Read Write Lock Unlock Add Done Wait Background WithCancel Join Split Contains TrimSpace Itoa Atoi Marshal Unmarshal Now Since"],
        declarers: &[("func", Decl::Function), ("var", Decl::Variable), ("const", Decl::Variable), ("type", Decl::Type)],
        run: Some("go run {file}"), color: 0x00add8, icon: "icons/files/go.svg", server: native("gopls", "go", Some("brew install gopls")), ..C_LIKE
    },
    Language {
        name: "Rust", highlight: "rust", extensions: &["rs"],
        statements: &["let fn use pub impl return match if mut struct enum for else self mod const trait where while loop break continue static type unsafe async await move ref crate super extern dyn in as"],
        globals: &["self Self Some None Ok Err String Vec Option Result Box Rc Arc HashMap HashSet println format vec assert assert_eq true false usize i32 u8 u32 u64 i64 f64 bool str char panic todo unimplemented Default Clone Debug"],
        members: &["unwrap clone iter map collect into to_string push len is_empty expect as_str get insert ok_or as_ref unwrap_or filter contains borrow lock join split trim parse new"],
        declarers: &[("let", Decl::Variable), ("fn", Decl::Function), ("struct", Decl::Type), ("enum", Decl::Type), ("trait", Decl::Type), ("type", Decl::Type), ("mod", Decl::Module), ("use", Decl::Module), ("const", Decl::Variable), ("static", Decl::Variable)],
        run: Some("rustc --edition 2024 {file} -o {out} && {out}"), color: 0xdea584, icon: "icons/files/rust.svg", server: native("rust-analyzer", "rust", Some("brew install rust-analyzer")), ..C_LIKE
    },
    Language {
        name: "PHP", highlight: "php", extensions: &["php"], word_chars: "$", line_comments: &["//", "#"], quotes: "\"'",
        statements: &["function return if public echo use private class else foreach as new static protected namespace require_once include try catch throw while for switch case break interface extends implements abstract final const elseif"],
        globals: &["$this null true false array self parent isset empty count strlen str_replace explode implode in_array array_map json_encode json_decode sprintf printf var_dump die"],
        declarers: &[("function", Decl::Function), ("class", Decl::Class), ("interface", Decl::Type), ("trait", Decl::Type), ("use", Decl::Module)],
        run: Some("php {file}"), color: 0x8892bf, icon: "icons/files/php.svg", server: npm("intelephense", "php", "npm install --prefix {dir} intelephense"), ..C_LIKE
    },
    Language {
        name: "Ruby", highlight: "ruby", extensions: &["rb", "rake", "gemspec"], word_chars: "?!@",
        statements: &["def end if do return class require module else elsif unless puts attr_accessor private self case when while until begin rescue ensure yield raise then include extend"],
        globals: &["nil true false self puts print require_relative attr_reader attr_writer lambda proc new"],
        members: &["each map select reject first last length size empty? include? push join split strip to_s to_i to_sym each_with_index inject keys values"],
        declarers: &[("def", Decl::Function), ("class", Decl::Class), ("module", Decl::Module)],
        run: Some("ruby {file}"), color: 0xcc342d, icon: "icons/files/ruby.svg", server: Some(Server { program: "ruby-lsp", args: &[], language_id: "ruby", install: None, init: None }), ..HASH_COMMENTS
    },
    Language {
        name: "Swift", highlight: "swift", extensions: &["swift"],
        statements: &["let var func return if import guard else class struct self enum case switch for in while protocol extension private public static override init throws try catch defer async await weak lazy"],
        globals: &["self nil true false String Int Double Bool Array Dictionary print Optional Any Void Error URL Data Date View some"],
        members: &["count isEmpty append map filter first last contains reduce sorted joined description forEach remove insert"],
        declarers: &[("let", Decl::Variable), ("var", Decl::Variable), ("func", Decl::Function), ("class", Decl::Class), ("struct", Decl::Type), ("enum", Decl::Type), ("protocol", Decl::Type), ("import", Decl::Module)],
        run: Some("swift {file}"), color: 0xf05138, icon: "icons/files/swift.svg", server: native("sourcekit-lsp", "swift", Some("xcode-select --install")), ..C_LIKE
    },
    Language {
        name: "Kotlin", highlight: "kotlin", extensions: &["kt", "kts"],
        statements: &["fun val var return if import class package else when private override object data for is in null this interface companion init try catch throw while sealed open suspend enum internal lateinit const"],
        globals: &["println String Int Boolean List listOf mutableListOf mapOf setOf null true false Unit Any Long Double arrayOf emptyList lazy require check"],
        members: &["let apply also run size isEmpty map filter forEach first last toString toInt get add contains joinToString"],
        declarers: &[("fun", Decl::Function), ("val", Decl::Variable), ("var", Decl::Variable), ("class", Decl::Class), ("interface", Decl::Type), ("object", Decl::Class), ("import", Decl::Module)],
        run: Some("kotlinc {file} -include-runtime -d {out}.jar && java -jar {out}.jar"), color: 0xa97bff, icon: "icons/files/kotlin.svg", server: native("kotlin-language-server", "kotlin", Some("brew install kotlin-language-server")), ..C_LIKE
    },
    Language {
        name: "Shell", highlight: "bash", extensions: &["sh", "bash", "zsh"], word_chars: "-",
        statements: &["if then fi else elif for do done while case esac function return export local echo exit source in"],
        globals: &["echo printf cd ls grep sed awk cat mkdir rm cp mv find chmod curl git read set test true false shift HOME PATH PWD USER"],
        declarers: &[("function", Decl::Function), ("export", Decl::Variable), ("local", Decl::Variable)],
        run: Some("bash {file}"), color: 0x89e051, icon: "icons/files/shell.svg", server: Some(Server { program: "bash-language-server", args: &["start"], language_id: "shellscript", install: Some("npm install --prefix {dir} bash-language-server"), init: None }), ..HASH_COMMENTS
    },
    Language {
        name: "PowerShell", highlight: "powershell", extensions: &["ps1", "psm1", "psd1"], word_chars: "-$", block_comment: Some(("<#", "#>")),
        statements: &["function param if else elseif foreach return try catch finally throw switch for while do until break continue exit begin process end filter class enum using Import-Module"],
        globals: &["$true $false $null $_ $PSScriptRoot $env $args $Error Write-Host Write-Output Write-Error Write-Warning Get-ChildItem Get-Content Set-Content Add-Content Get-Item Test-Path New-Item Remove-Item Copy-Item Move-Item Join-Path Split-Path Resolve-Path Get-Location Set-Location Get-Process Start-Process Stop-Process Get-Service Invoke-WebRequest Invoke-RestMethod Invoke-Expression ConvertTo-Json ConvertFrom-Json Select-Object Where-Object ForEach-Object Sort-Object Group-Object Measure-Object Out-File Out-Null Get-Date Read-Host Get-Command Get-Help Get-Member New-Object -Path -Force -Recurse -Filter -Name -Value -ErrorAction -eq -ne -gt -lt -ge -le -like -match -and -or -not"],
        members: &["Length Count Name FullName ToString Split Trim Replace Contains Substring ToUpper ToLower StartsWith EndsWith Add Remove Keys Values Exists Extension BaseName"],
        declarers: &[("function", Decl::Function), ("class", Decl::Class), ("enum", Decl::Type), ("filter", Decl::Function)],
        run: Some("pwsh -File {file}"), color: 0x5391fe, icon: "icons/files/powershell.svg", ..HASH_COMMENTS
    },
    // Also `Dockerfile`, `Dockerfile.dev`, `Containerfile` (see `for_path`).
    Language {
        name: "Dockerfile", highlight: "dockerfile", extensions: &["dockerfile", "containerfile"], word_chars: "-",
        statements: &["FROM RUN COPY WORKDIR ENV EXPOSE CMD ENTRYPOINT ARG ADD LABEL USER VOLUME HEALTHCHECK SHELL ONBUILD STOPSIGNAL"],
        globals: &["AS apt-get install update upgrade -y --no-install-recommends rm -rf /var/lib/apt/lists/* apk add --no-cache npm ci pip --no-cache-dir node python alpine ubuntu debian slim latest --from --chown --platform /app && echo mkdir cd"],
        declarers: &[("ARG", Decl::Variable), ("ENV", Decl::Variable), ("FROM", Decl::Module)],
        run: Some("docker build -f {file} ."), color: 0x2496ed, icon: "icons/files/docker.svg",
        server: npm("docker-langserver", "dockerfile", "npm install --prefix {dir} dockerfile-language-server-nodejs"), ..HASH_COMMENTS
    },
    // SQL keywords appear anywhere in a line: all in `globals`.
    Language {
        name: "SQL", highlight: "sql", extensions: &["sql"], code: true, line_comments: &["--"], block_comment: Some(("/*", "*/")), quotes: "'",
        globals: &["SELECT FROM WHERE AND INSERT INTO VALUES UPDATE SET DELETE CREATE TABLE JOIN ON AS OR NOT NULL ORDER BY GROUP LIMIT LEFT INNER IS IN LIKE DISTINCT COUNT SUM AVG MIN MAX ALTER DROP HAVING UNION INDEX PRIMARY KEY FOREIGN REFERENCES DEFAULT UNIQUE CASE WHEN THEN ELSE END INTEGER TEXT VARCHAR BOOLEAN TIMESTAMP"],
        declarers: &[("TABLE", Decl::Type), ("VIEW", Decl::Type), ("INDEX", Decl::Type)],
        run: None, color: 0xe38c00, icon: "icons/files/sql.svg", ..BASE
    },
    // ---------- docs & config ----------
    Language {
        name: "Markdown", highlight: "markdown", extensions: &["md", "markdown", "mdx"],
        color: 0x6cb6ff, icon: "icons/files/markdown.svg", ..BASE
    },
    Language {
        name: "YAML", highlight: "yaml", extensions: &["yaml", "yml"], line_comments: &["#"], quotes: "\"'", globals: &["true false null"],
        color: 0xcb171e, icon: "icons/files/config.svg", server: npm("yaml-language-server", "yaml", "npm install --prefix {dir} yaml-language-server"), ..BASE
    },
    // ---------- more web frameworks (HTML highlighting for their templates) ----------
    Language {
        name: "Vue", highlight: "html", extensions: &["vue"], word_chars: "-$",
        statements: &[JS_STMT], globals: &[JS_EXPR, HTML], members: &[JS_MEMBERS], declarers: JS_DECL,
        color: 0x41b883, icon: "icons/files/vue.svg",
        server: Some(Server { program: "vue-language-server", args: &["--stdio"], language_id: "vue", install: Some("npm install --prefix {dir} @vue/language-server@2 typescript@5"), init: Some(lsp::tsdk_options) }),
        ..C_LIKE
    },
    Language {
        name: "Svelte", highlight: "html", extensions: &["svelte"], word_chars: "-$",
        statements: &[JS_STMT], globals: &[JS_EXPR, HTML], members: &[JS_MEMBERS], declarers: JS_DECL,
        color: 0xff3e00, icon: "icons/files/svelte.svg", server: npm("svelteserver", "svelte", "npm install --prefix {dir} svelte-language-server"), ..C_LIKE
    },
    Language {
        name: "Astro", highlight: "html", extensions: &["astro"], word_chars: "-$",
        statements: &[JS_STMT], globals: &[JS_EXPR, HTML], members: &[JS_MEMBERS], declarers: JS_DECL,
        color: 0xbc52ee, icon: "icons/files/astro.svg",
        server: Some(Server { program: "astro-ls", args: &["--stdio"], language_id: "astro", install: Some("npm install --prefix {dir} @astrojs/language-server typescript@5"), init: Some(lsp::tsdk_options) }),
        ..C_LIKE
    },
    Language {
        name: "Prisma", extensions: &["prisma"],
        statements: &["model enum datasource generator type view"], globals: &["String Int Boolean DateTime Float Decimal Json Bytes BigInt @id @default @unique @relation @map @@map @@index autoincrement now uuid cuid env"],
        color: 0x5a67d8, icon: "icons/files/prisma.svg", server: npm("prisma-language-server", "prisma", "npm install --prefix {dir} @prisma/language-server"), ..C_LIKE
    },
    Language {
        name: "GraphQL", highlight: "graphql", extensions: &["graphql", "gql"],
        statements: &["query mutation subscription fragment type input enum interface union scalar schema extend on"], globals: &["String Int Float Boolean ID true false null"],
        color: 0xe10098, icon: "icons/files/graphql.svg",
        server: Some(Server { program: "graphql-lsp", args: &["server", "-m", "stream"], language_id: "graphql", install: Some("npm install --prefix {dir} graphql-language-service-cli graphql"), init: None }),
        ..HASH_COMMENTS
    },
    // ---------- more languages ----------
    Language {
        name: "Terraform", highlight: "hcl", extensions: &["tf", "tfvars", "hcl"],
        statements: &["resource data variable output module provider terraform locals"], globals: &["var local module data true false null count each self"],
        line_comments: &["#", "//"], block_comment: Some(("/*", "*/")),
        color: 0x7b42bc, icon: "icons/files/terraform.svg", server: Some(Server { program: "terraform-ls", args: &["serve"], language_id: "terraform", install: Some("brew install hashicorp/tap/terraform-ls"), init: None }), ..HASH_COMMENTS
    },
    Language {
        name: "Lua", highlight: "lua", extensions: &["lua"],
        line_comments: &["--"], block_comment: Some(("--[[", "]]")), quotes: "\"'",
        statements: &["local function return if then end else elseif for in do while repeat until break"], globals: &["nil true false print require pairs ipairs table string math tostring tonumber type self"],
        declarers: &[("local", Decl::Variable), ("function", Decl::Function)],
        run: Some("lua {file}"), color: 0x000080, icon: "icons/files/lua.svg", server: native("lua-language-server", "lua", Some("brew install lua-language-server")), code: true, ..BASE
    },
    Language {
        name: "Zig", highlight: "zig", extensions: &["zig", "zon"],
        statements: &["const var fn pub return if else while for switch try catch defer errdefer comptime struct enum union error test break continue"], globals: &["std @import true false null undefined u8 u32 i32 usize void bool anytype"],
        declarers: &[("const", Decl::Variable), ("var", Decl::Variable), ("fn", Decl::Function)],
        run: Some("zig run {file}"), color: 0xf7a41d, icon: "icons/files/zig.svg", server: native("zls", "zig", Some("brew install zls")), ..C_LIKE
    },
    Language {
        name: "Elixir", highlight: "elixir", extensions: &["ex", "exs"], word_chars: "?!",
        statements: &["def defp defmodule do end if else case cond with fn import alias use require when receive raise"], globals: &["nil true false IO Enum Map List String Kernel self"],
        declarers: &[("def", Decl::Function), ("defp", Decl::Function), ("defmodule", Decl::Module)],
        run: Some("elixir {file}"), color: 0x6e4a7e, icon: "icons/files/elixir.svg", server: native("elixir-ls", "elixir", Some("brew install elixir-ls")), ..HASH_COMMENTS
    },
    Language {
        name: "TOML", highlight: "toml", extensions: &["toml"], line_comments: &["#"], quotes: "\"'", globals: &["true false"],
        color: 0x9c4221, icon: "icons/files/config.svg", ..BASE
    },
];

pub static PLAIN: Language = BASE;

pub fn for_path(path: &Path) -> &'static Language {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_lowercase();
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or_default().to_lowercase();
    // Named, not suffixed: `Dockerfile`, `Dockerfile.prod`, `Containerfile`.
    let key = ["dockerfile", "containerfile"].into_iter().find(|n| name == *n || name.starts_with(&format!("{n}."))).unwrap_or(&ext);
    LANGUAGES.iter().find(|l| l.extensions.contains(&key)).unwrap_or(&PLAIN)
}

pub fn is_markdown(lang: &Language) -> bool {
    lang.highlight == "markdown"
}

/// Grammars gpui-component doesn't bundle.
pub fn register_grammars() {
    let registry = LanguageRegistry::singleton();
    registry.register(
        "php",
        &LanguageConfig::new("php", tree_sitter_php::LANGUAGE_PHP.into(), vec!["html".into()], tree_sitter_php::HIGHLIGHTS_QUERY, tree_sitter_php::INJECTIONS_QUERY, ""),
    );
    registry.register(
        "powershell",
        &LanguageConfig::new("powershell", tree_sitter_powershell::LANGUAGE.into(), vec![], tree_sitter_powershell::HIGHLIGHTS_QUERY, "", ""),
    );
    registry.register(
        "dockerfile",
        &LanguageConfig::new(
            "dockerfile",
            tree_sitter_containerfile::LANGUAGE.into(),
            vec!["bash".into(), "json".into(), "yaml".into(), "toml".into()],
            tree_sitter_containerfile::HIGHLIGHTS_QUERY,
            tree_sitter_containerfile::INJECTIONS_QUERY,
            "",
        ),
    );
    registry.register("lua", &LanguageConfig::new("lua", tree_sitter_lua::LANGUAGE.into(), vec![], tree_sitter_lua::HIGHLIGHTS_QUERY, tree_sitter_lua::INJECTIONS_QUERY, ""));
    registry.register("hcl", &LanguageConfig::new("hcl", tree_sitter_hcl::LANGUAGE.into(), vec![], HCL_HIGHLIGHTS, "", ""));
    registry.register(
        "kotlin",
        &LanguageConfig::new("kotlin", tree_sitter_kotlin_ng::LANGUAGE.into(), vec![], KOTLIN_HIGHLIGHTS, "", ""),
    );
}

/// tree-sitter-kotlin-ng ships no highlight query.
pub const KOTLIN_HIGHLIGHTS: &str = include_str!("../assets/queries/kotlin.scm");
/// tree-sitter-hcl ships no highlight query either.
pub const HCL_HIGHLIGHTS: &str = include_str!("../assets/queries/hcl.scm");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_language_is_reachable_and_has_its_icon() {
        for lang in LANGUAGES {
            for ext in lang.extensions {
                assert_eq!(for_path(Path::new(&format!("a.{ext}"))).name, lang.name, "extension .{ext} claimed twice");
            }
            assert!(crate::icons::Assets::get(lang.icon).is_some(), "{} icon missing: {}", lang.name, lang.icon);
        }
        assert_eq!(for_path(Path::new("x.unknown")).name, "Texte");
        for (file, name) in [("App.vue", "Vue"), ("main.tf", "Terraform"), ("init.lua", "Lua"), ("schema.prisma", "Prisma"), ("q.gql", "GraphQL"), ("a.exs", "Elixir"), ("b.zig", "Zig")] {
            assert_eq!(for_path(Path::new(file)).name, name);
        }
        for name in ["Dockerfile", "dockerfile.prod", "Containerfile", "api.dockerfile"] {
            assert_eq!(for_path(Path::new(name)).name, "Dockerfile", "{name}");
        }
        assert_eq!(for_path(Path::new("dockerfiles.md")).name, "Markdown");
    }

    #[test]
    fn added_grammars_compile_with_their_queries() {
        for (lang, query) in [
            (tree_sitter::Language::from(tree_sitter_php::LANGUAGE_PHP), tree_sitter_php::HIGHLIGHTS_QUERY),
            (tree_sitter_kotlin_ng::LANGUAGE.into(), KOTLIN_HIGHLIGHTS),
            (tree_sitter_powershell::LANGUAGE.into(), tree_sitter_powershell::HIGHLIGHTS_QUERY),
            (tree_sitter_containerfile::LANGUAGE.into(), tree_sitter_containerfile::HIGHLIGHTS_QUERY),
            (tree_sitter_containerfile::LANGUAGE.into(), tree_sitter_containerfile::INJECTIONS_QUERY),
            (tree_sitter_lua::LANGUAGE.into(), tree_sitter_lua::HIGHLIGHTS_QUERY),
            (tree_sitter_hcl::LANGUAGE.into(), HCL_HIGHLIGHTS),
        ] {
            tree_sitter::Query::new(&lang, query).unwrap();
        }
    }
}
