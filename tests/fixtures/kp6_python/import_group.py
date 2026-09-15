"""KP-6-python curated snippet: import-group adjacency.

A module-level import group: three adjacent imports, then a comment
(an import-section note, the shape linters ask for), then one more
import. Runs of (import_statement, import_statement): strict sees 2
(json,os) and (os,sys); skip sees 3 — the (sys,re) pair reappears once
the comment is skipped.
"""

import json
import os
import sys
# comment inside the import group
import re
