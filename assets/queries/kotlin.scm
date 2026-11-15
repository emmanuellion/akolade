; Minimal Kotlin highlighting (tree-sitter-kotlin-ng ships no query).

(line_comment) @comment
(block_comment) @comment
(string_literal) @string
(multiline_string_literal) @string
(character_literal) @string
(number_literal) @number
(float_literal) @number
(annotation) @attribute
(label) @label

(function_declaration name: (_) @function)
(class_declaration name: (_) @type)
(call_expression (identifier) @function)
(navigation_expression (identifier) @property)

[
  "abstract" "as" "by" "catch" "class" "companion" "const" "constructor" "data" "do"
  "else" "enum" "finally" "for" "fun" "get" "if" "import" "in" "init" "inline"
  "interface" "internal" "is" "lateinit" "object" "open" "operator" "out" "override"
  "package" "private" "protected" "public" "return" "sealed" "set" "super" "suspend"
  "this" "throw" "try" "typealias" "val" "var" "vararg" "when" "where" "while"
] @keyword
