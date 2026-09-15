// Command demo consumes the shapes package end to end.
package main

import (
	"fmt"

	"example.com/shapes"
)

func main() {
	raw := "  MX-2000 "
	clean := shapes.NormalizeCode(raw)
	folded := shapes.FoldCode(clean, "mx-2001")
	fmt.Println(clean, folded, shapes.Describe([]string{raw, "mx-2002"}))
}
