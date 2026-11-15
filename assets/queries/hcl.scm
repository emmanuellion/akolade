; HCL / Terraform highlights (tree-sitter-hcl ships none).
(comment) @comment
(block (identifier) @keyword)
(block (string_lit) @type)
(attribute (identifier) @property)
(object_elem key: (expression (variable_expr (identifier) @property)))
(quoted_template) @string
(template_literal) @string
(heredoc_template) @string
(numeric_lit) @number
(bool_lit) @boolean
(null_lit) @constant
(function_call (identifier) @function)
(variable_expr (identifier) @variable)
(get_attr (identifier) @property)
(template_interpolation_start) @punctuation.special
(template_interpolation_end) @punctuation.special
