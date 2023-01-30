package client

import "errors"

// ErrMissingIDMappings gets returned if user namespace unsharing is selected
// but no IDMappings being provided.
var ErrMissingIDMappings = errors.New("unsharing user namespace selected but no IDMappings provided")
