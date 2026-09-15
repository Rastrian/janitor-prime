// KP-6-go curated snippet: struct-tag-adjacent-field.
//
// Four tagged fields (the field_declaration + raw_string_literal tag
// shape). The first two are adjacent, the last two are adjacent, and a
// field-group comment divides the groups: strict sees two pairs, skip
// sees three.

package kp6

type Labeled struct {
	ID    int    `json:"id"`
	Score int    `json:"score"`
	// presentation fields follow
	Name  string `json:"name"`
	Notes string `json:"notes"`
}
