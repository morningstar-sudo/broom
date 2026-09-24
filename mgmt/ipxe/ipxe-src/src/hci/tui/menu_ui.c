/*
 * Copyright (C) 2012 Michael Brown <mbrown@fensystems.co.uk>.
 *
 * This program is free software; you can redistribute it and/or
 * modify it under the terms of the GNU General Public License as
 * published by the Free Software Foundation; either version 2 of the
 * License, or any later version.
 *
 * This program is distributed in the hope that it will be useful, but
 * WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the GNU
 * General Public License for more details.
 *
 * You should have received a copy of the GNU General Public License
 * along with this program; if not, write to the Free Software
 * Foundation, Inc., 51 Franklin Street, Fifth Floor, Boston, MA
 * 02110-1301, USA.
 *
 * You can also choose to distribute this program under the terms of
 * the Unmodified Binary Distribution Licence (as given in the file
 * COPYING.UBDL), provided that you have satisfied its requirements.
 */

FILE_LICENCE ( GPL2_OR_LATER_OR_UBDL );
FILE_SECBOOT ( PERMITTED );

/** @file
 *
 * Menu interface
 *
 */

#include <stdio.h>
#include <string.h>
#include <stdlib.h>
#include <errno.h>
#include <curses.h>
#include <ipxe/keys.h>
#include <ipxe/timer.h>
#include <ipxe/console.h>
#include <ipxe/ansicol.h>
#include <ipxe/jumpscroll.h>
#include <ipxe/dynui.h>
#include <ipxe/settings.h>

/* Screen layout (Broom):
 *
 *   row 1        title (left, bold)
 *   row 2        menu-hint setting (left) + countdown (right)
 *   row 3        horizontal rule
 *   row 5..      items (fixed-width highlight bar)
 *   LINES-3      horizontal rule
 *   LINES-2      menu-footer setting: "left|centre|right"
 */
#define EDGE_COL	2U
#define TITLE_ROW	1U
#define HINT_ROW	2U
#define RULE_ROW	3U
#define MENU_ROW	5U
#define MENU_COL	6U
#define FOOTER_RULE_ROW	( LINES - 3U )
#define FOOTER_ROW	( LINES - 2U )
#define MENU_ROWS	( FOOTER_RULE_ROW - 1U - MENU_ROW )
#define MENU_COLS	( COLS - MENU_COL - EDGE_COL )
#define MENU_PAD	2U
#define MENU_MIN_WIDTH	30U
#define TIMEOUT_WIDTH	32U

/** A menu user interface */
struct menu_ui {
	/** Dynamic user interface */
	struct dynamic_ui *dynui;
	/** Jump scroller */
	struct jump_scroller scroll;
	/** Remaining timeout (0=indefinite) */
	unsigned long timeout;
	/** Post-activity timeout (0=indefinite) */
	unsigned long retimeout;
	/** Width of highlight bar */
	unsigned int width;
};

/** Hint text under the title: `set menu-hint <text>` before `choose` */
const struct setting menu_hint_setting __setting ( SETTING_MISC, menu-hint ) = {
	.name = "menu-hint",
	.description = "Menu hint line",
	.type = &setting_type_string,
};

/** Footer text: `set menu-footer <left>|<centre>|<right>` before `choose` */
const struct setting menu_footer_setting __setting ( SETTING_MISC, menu-footer ) = {
	.name = "menu-footer",
	.description = "Menu footer line",
	.type = &setting_type_string,
};

/**
 * Draw a numbered menu item
 *
 * @v ui		Menu user interface
 * @v index		Index
 */
static void draw_menu_item ( struct menu_ui *ui, unsigned int index ) {
	struct dynamic_item *item;
	unsigned int row_offset;
	char buf[ MENU_COLS + 1 /* NUL */ ];
	size_t max_len;
	size_t len;

	/* Move to start of row */
	row_offset = ( index - ui->scroll.first );
	move ( ( MENU_ROW + row_offset ), MENU_COL );

	/* Get menu item */
	item = dynui_item ( ui->dynui, index );
	if ( item ) {

		/* Draw separators in a different colour */
		if ( ! item->name )
			color_set ( CPAIR_SEPARATOR, NULL );

		/* Highlight if this is the selected item */
		if ( index == ui->scroll.current ) {
			color_set ( CPAIR_SELECT, NULL );
			attron ( A_BOLD );
		}

		/* Construct row */
		memset ( buf, ' ', ui->width );
		buf[ui->width] = '\0';
		len = strlen ( item->text );
		max_len = ( ui->width - ( 2 * MENU_PAD ) );
		if ( len > max_len )
			len = max_len;
		memcpy ( ( buf + MENU_PAD ), item->text, len );

		/* Print row */
		printw ( "%s", buf );

		/* Reset attributes */
		color_set ( CPAIR_NORMAL, NULL );
		attroff ( A_BOLD );

	} else {
		/* Clear row if there is no corresponding menu item */
		clrtoeol();
	}

	/* Move cursor back to start of row */
	move ( ( MENU_ROW + row_offset ), MENU_COL );
}

/**
 * Draw the current block of menu items
 *
 * @v ui		Menu user interface
 */
static void draw_menu_items ( struct menu_ui *ui ) {
	unsigned int i;

	/* Draw ellipses before and/or after the list as necessary */
	color_set ( CPAIR_SEPARATOR, NULL );
	mvaddstr ( ( MENU_ROW - 1 ), ( MENU_COL + MENU_PAD ),
		   ( jump_scroll_is_first ( &ui->scroll ) ? "   " : "..." ) );
	mvaddstr ( ( MENU_ROW + MENU_ROWS ), ( MENU_COL + MENU_PAD ),
		   ( jump_scroll_is_last ( &ui->scroll ) ? "   " : "..." ) );
	color_set ( CPAIR_NORMAL, NULL );

	/* Draw visible items */
	for ( i = 0 ; i < MENU_ROWS ; i++ )
		draw_menu_item ( ui, ( ui->scroll.first + i ) );
}

/**
 * Draw countdown at the right of the hint row (blank once cancelled)
 *
 * @v ui		Menu user interface
 */
static void draw_menu_timeout ( struct menu_ui *ui ) {
	char text[ TIMEOUT_WIDTH + 1 /* NUL */ ];
	char buf[ TIMEOUT_WIDTH + 1 /* NUL */ ];
	size_t len = 0;

	if ( ui->timeout ) {
		len = snprintf ( text, sizeof ( text ),
				 "Auto boot in %ld seconds",
				 ( ( ui->timeout + TICKS_PER_SEC - 1 ) /
				   TICKS_PER_SEC ) );
		if ( len > TIMEOUT_WIDTH )
			len = TIMEOUT_WIDTH;
	}
	memset ( buf, ' ', TIMEOUT_WIDTH );
	buf[TIMEOUT_WIDTH] = '\0';
	memcpy ( ( buf + TIMEOUT_WIDTH - len ), text, len );
	mvprintw ( HINT_ROW, ( COLS - EDGE_COL - TIMEOUT_WIDTH ), "%s", buf );
}

/**
 * Draw a full-width horizontal rule
 *
 * @v row		Screen row
 *
 * Emits U+2500 as UTF-8 directly (the EFI and framebuffer consoles
 * decode UTF-8; curses only handles single-byte characters), then
 * invalidates the curses cursor so the next draw repositions itself.
 */
static void draw_menu_rule ( unsigned int row ) {
	unsigned int col;

	color_set ( CPAIR_SEPARATOR, NULL );
	mvaddch ( row, EDGE_COL, ' ' );	/* push colour to the screen */
	printf ( "\033[%d;%dH", ( row + 1 ), ( EDGE_COL + 1 ) );
	for ( col = EDGE_COL ; col < ( COLS - EDGE_COL ) ; col++ )
		printf ( "\xe2\x94\x80" );
	stdscr->scr->curs_x = ~0U;
	color_set ( CPAIR_NORMAL, NULL );
}

/**
 * Draw '|'-separated setting text on one row: first field left, last
 * field right, a middle field centred
 *
 * @v setting		Setting
 * @v row		Screen row
 */
static void draw_menu_fields ( const struct setting *setting,
			       unsigned int row ) {
	char *text = NULL;
	char *field[3];
	unsigned int count = 0;
	unsigned int max_len;
	unsigned int len;
	unsigned int col;
	unsigned int i;
	char *c;

	if ( fetch_string_setting_copy ( NULL, setting, &text ) > 0 ) {
		field[count++] = text;
		for ( c = text ; *c ; c++ ) {
			if ( ( *c == '|' ) && ( count < 3 ) ) {
				*c = '\0';
				field[count++] = ( c + 1 );
			}
		}
		max_len = ( ( COLS - ( 2 * EDGE_COL ) ) / count );
		for ( i = 0 ; i < count ; i++ ) {
			len = strlen ( field[i] );
			if ( len > max_len ) {
				len = max_len;
				field[i][len] = '\0';
			}
			if ( i == 0 ) {
				col = EDGE_COL;
			} else if ( i == ( count - 1 ) ) {
				col = ( COLS - EDGE_COL - len );
			} else {
				col = ( ( COLS - len ) / 2 );
			}
			mvprintw ( row, col, "%s", field[i] );
		}
	}
	free ( text );
}

/**
 * Menu main loop
 *
 * @v ui		Menu user interface
 * @ret selected	Selected item
 * @ret rc		Return status code
 */
static int menu_loop ( struct menu_ui *ui, struct dynamic_item **selected ) {
	struct dynamic_item *item;
	unsigned long timeout;
	unsigned int previous;
	unsigned int move;
	int key;
	int chosen = 0;
	int rc = 0;

	do {
		/* Record current selection */
		previous = ui->scroll.current;

		/* Calculate timeout as remainder of current second */
		timeout = ( ui->timeout % TICKS_PER_SEC );
		if ( ( timeout == 0 ) && ( ui->timeout != 0 ) )
			timeout = TICKS_PER_SEC;
		ui->timeout -= timeout;

		/* Get key */
		move = SCROLL_NONE;
		key = getkey ( timeout );
		if ( key < 0 ) {
			/* Choose default if we finally time out */
			if ( ui->timeout == 0 )
				chosen = 1;
		} else {
			/* Reset timeout after activity */
			ui->timeout = ui->retimeout;

			/* Handle scroll keys */
			move = jump_scroll_key ( &ui->scroll, key );

			/* Handle other keys */
			switch ( key ) {
			case ESC:
			case CTRL_C:
				rc = -ECANCELED;
				break;
			case CR:
			case LF:
				chosen = 1;
				break;
			default:
				item = dynui_shortcut ( ui->dynui, key,
							ui->scroll.current );
				if ( item ) {
					ui->scroll.current = item->index;
					if ( item->name ) {
						chosen = 1;
					} else {
						move = SCROLL_DOWN;
					}
				}
				break;
			}
		}

		/* Move selection, if applicable */
		while ( move ) {
			move = jump_scroll_move ( &ui->scroll, move );
			item = dynui_item ( ui->dynui, ui->scroll.current );
			if ( item->name )
				break;
		}

		/* Redraw selection and countdown if necessary */
		if ( ( ui->scroll.current != previous ) || ( timeout != 0 ) ) {
			draw_menu_item ( ui, previous );
			if ( jump_scroll ( &ui->scroll ) )
				draw_menu_items ( ui );
			draw_menu_item ( ui, ui->scroll.current );
			draw_menu_timeout ( ui );
		}

		/* Record selection */
		item = dynui_item ( ui->dynui, ui->scroll.current );
		assert ( item != NULL );
		assert ( item->name != NULL );
		*selected = item;

	} while ( ( rc == 0 ) && ! chosen );

	return rc;
}

/**
 * Show menu
 *
 * @v dynui		Dynamic user interface
 * @v timeout		Initial timeout period, in ticks (0=indefinite)
 * @v retimeout		Post-activity timeout period, in ticks (0=indefinite)
 * @ret selected	Selected item
 * @ret rc		Return status code
 */
int show_menu ( struct dynamic_ui *dynui, unsigned long timeout,
		unsigned long retimeout, const char *select,
		struct dynamic_item **selected ) {
	struct dynamic_item *item;
	struct menu_ui ui;
	char buf[ MENU_COLS + 1 /* NUL */ ];
	unsigned int width;
	int named_count = 0;
	int rc;

	/* Initialise UI */
	memset ( &ui, 0, sizeof ( ui ) );
	ui.dynui = dynui;
	ui.scroll.rows = MENU_ROWS;
	ui.timeout = timeout;
	ui.retimeout = retimeout;
	ui.width = MENU_MIN_WIDTH;

	list_for_each_entry ( item, &dynui->items, list ) {
		if ( item->name ) {
			if ( ! named_count )
				ui.scroll.current = ui.scroll.count;
			named_count++;
			if ( select ) {
				if ( strcmp ( select, item->name ) == 0 )
					ui.scroll.current = ui.scroll.count;
			} else {
				if ( item->flags & DYNUI_DEFAULT )
					ui.scroll.current = ui.scroll.count;
			}
		}
		width = ( strlen ( item->text ) + ( 2 * MENU_PAD ) );
		if ( width > ui.width )
			ui.width = width;
		ui.scroll.count++;
	}
	if ( ! named_count ) {
		/* Menus with no named items cannot be selected from,
		 * and will seriously confuse the navigation logic.
		 * Refuse to display any such menus.
		 */
		return -ENOENT;
	}
	if ( ui.width > MENU_COLS )
		ui.width = MENU_COLS;

	/* Initialise screen */
	initscr();
	start_color();
	color_set ( CPAIR_NORMAL, NULL );
	curs_set ( 0 );
	erase();

	/* Draw initial content */
	draw_menu_rule ( RULE_ROW );
	draw_menu_rule ( FOOTER_RULE_ROW );
	attron ( A_BOLD );
	snprintf ( buf, sizeof ( buf ), "%s", ui.dynui->title );
	mvprintw ( TITLE_ROW, EDGE_COL, "%s", buf );
	attroff ( A_BOLD );
	draw_menu_fields ( &menu_hint_setting, HINT_ROW );
	draw_menu_fields ( &menu_footer_setting, FOOTER_ROW );
	draw_menu_timeout ( &ui );
	jump_scroll ( &ui.scroll );
	draw_menu_items ( &ui );
	draw_menu_item ( &ui, ui.scroll.current );

	/* Enter main loop */
	rc = menu_loop ( &ui, selected );
	assert ( *selected );

	/* Clear screen */
	endwin();

	return rc;
}
