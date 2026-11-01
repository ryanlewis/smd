# smd help

Go back to leave this page and return to where you were. The keys below are the ones bound now, including any you changed in the config.

## Keys

{keys}

## Moving around

Type a number before a key to repeat it: with the default keys, `5j` scrolls five lines and `3` Ctrl-D three half pages. Before top or bottom (`g` or `G`), the number is a line to go to, so `42G` puts line 42 at the top. A line is a row of the note as smd draws it, counting from 1. The number shows on the bottom row as you type it, and Esc drops it.

The bottom row shows which page is in view, and says when something fails, such as a link that leads nowhere, until the next key.

## Search

Press search and type a term. Matches are marked as you type, and the bottom row says how many there are. Enter goes to the first match from the top of the screen; the next and previous match keys go round at the end. Esc while typing gives the search up, and Esc after it clears the marks. The search ignores case unless the term has a capital letter in it.

## Links, Back and Forward

Tab and Shift-Tab select links, checkboxes and folding titles; Enter follows a link, ticks a checkbox or folds a title. A link to another note opens it in smd, and Back returns to where you were. Web links and files smd does not show are opened with the system's opener. The toolbar's Back and Forward buttons can be clicked too.

## Tasks

A selected checkbox is ticked or unticked in the file itself. smd changes only the one character between the brackets. Checkboxes in a file smd cannot write to, or in a note read from stdin, are passed over.

## Reloading

smd reloads the file when it changes on disk, and reload reads it again by hand. A note read from stdin, and this page, have nothing to reload.

## Config

{config}

Every setting, and the names of the keys, are in the [README](https://github.com/ryanlewis/smd#config).
