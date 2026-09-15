// KP-6-go curated snippet: grouped-import block.
//
// A grouped import declaration: three adjacent import_specs, then a
// section comment (where linters ask side-effectful imports to sit),
// then one more. Runs of (import_spec, import_spec): strict sees two,
// skip sees three.

package kp6

import (
	"errors"
	"fmt"
	"strings"
	// side-effectful imports kept last
	_ "embed"
)

func Use() error { return errors.New(fmt.Sprintf("%s", strings.TrimSpace("x"))) }
