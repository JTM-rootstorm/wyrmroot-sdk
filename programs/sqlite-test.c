/* SPDX-License-Identifier: GPL-3.0-or-later */
/*
 * SQLite's test on Wyrmroot: the library through its C API, and the C
 * library under it (files on /, seeks, sync, truncate, unlink, mkdir as the
 * lock, the clock, and malloc). Any failed check prints what failed and exits
 * with status 1.
 *
 * The SDK builds SQLite with the unix-dotfile VFS as its default: Wyrmroot
 * has no POSIX record locks yet, so a lock is a `<database>.lock` directory.
 */
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#include "sqlite3.h"

static const char *const PATH = "/sqlite-test.db";
static const char *const LOCK = "/sqlite-test.db.lock";
static int checks;

static void check(int condition, const char *what)
{
	checks++;
	if (!condition) {
		fprintf(stderr, "sqlite-test: check failed: %s\n", what);
		exit(1);
	}
}

static sqlite3 *open_database(void)
{
	sqlite3 *db = 0;
	int rc = sqlite3_open(PATH, &db);
	if (rc != SQLITE_OK) {
		fprintf(stderr, "sqlite-test: open: %s\n", db ? sqlite3_errmsg(db) : "no memory");
		exit(1);
	}
	return db;
}

/* Runs SQL that must succeed. */
static void exec(sqlite3 *db, const char *sql)
{
	char *error = 0;
	if (sqlite3_exec(db, sql, 0, 0, &error) != SQLITE_OK) {
		fprintf(stderr, "sqlite-test: %s: %s\n", sql, error ? error : "?");
		exit(1);
	}
}

/* The first column of the first row, as an integer. */
static sqlite3_int64 integer(sqlite3 *db, const char *sql)
{
	sqlite3_stmt *statement;
	if (sqlite3_prepare_v2(db, sql, -1, &statement, 0) != SQLITE_OK
	    || sqlite3_step(statement) != SQLITE_ROW) {
		fprintf(stderr, "sqlite-test: %s: %s\n", sql, sqlite3_errmsg(db));
		exit(1);
	}
	sqlite3_int64 value = sqlite3_column_int64(statement, 0);
	sqlite3_finalize(statement);
	return value;
}

/* The first column of the first row, as text compared with `expected`. */
static int text_is(sqlite3 *db, const char *sql, const char *expected)
{
	sqlite3_stmt *statement;
	if (sqlite3_prepare_v2(db, sql, -1, &statement, 0) != SQLITE_OK
	    || sqlite3_step(statement) != SQLITE_ROW) {
		fprintf(stderr, "sqlite-test: %s: %s\n", sql, sqlite3_errmsg(db));
		exit(1);
	}
	const unsigned char *value = sqlite3_column_text(statement, 0);
	int same = value && strcmp((const char *)value, expected) == 0;
	if (!same)
		fprintf(stderr, "sqlite-test: %s gave \"%s\", not \"%s\"\n", sql,
			value ? (const char *)value : "(null)", expected);
	sqlite3_finalize(statement);
	return same;
}

int main(void)
{
	unlink(PATH);
	sqlite3 *db = open_database();
	check(strcmp(sqlite3_libversion(), SQLITE_VERSION) == 0, "library version");
	check(text_is(db, "PRAGMA journal_mode", "delete"), "rollback journal");

	/* A table of 20000 rows through one prepared statement, in one
	 * transaction (journal file, fsync, commit). */
	exec(db, "CREATE TABLE item(id INTEGER PRIMARY KEY, name TEXT, price REAL, kind INT)");
	exec(db, "BEGIN");
	sqlite3_stmt *insert;
	check(sqlite3_prepare_v2(db, "INSERT INTO item(name, price, kind) VALUES (?, ?, ?)", -1,
				 &insert, 0) == SQLITE_OK, "prepare insert");
	for (int i = 1; i <= 20000; i++) {
		char name[32];
		snprintf(name, sizeof name, "item-%05d", (i * 7919) % 20000);
		sqlite3_bind_text(insert, 1, name, -1, SQLITE_TRANSIENT);
		sqlite3_bind_double(insert, 2, i * 0.25);
		sqlite3_bind_int(insert, 3, i % 7);
		if (sqlite3_step(insert) != SQLITE_DONE)
			check(0, "insert row");
		sqlite3_reset(insert);
	}
	sqlite3_finalize(insert);
	exec(db, "COMMIT");
	check(integer(db, "SELECT count(*) FROM item") == 20000, "row count");
	check(integer(db, "SELECT sum(kind) FROM item") == 59998, "sum");
	check(text_is(db, "SELECT printf('%.1f', avg(price) * 8) FROM item", "20001.0"), "float average");

	/* Indexes, sorting, grouping and joins. */
	exec(db, "CREATE INDEX item_name ON item(name)");
	check(text_is(db, "SELECT name FROM item ORDER BY name LIMIT 1 OFFSET 9999", "item-09999"),
	      "ordered by index");
	check(integer(db, "SELECT count(*) FROM (SELECT kind FROM item GROUP BY kind)") == 7, "group by");
	exec(db, "CREATE TABLE kind(id INTEGER PRIMARY KEY, label TEXT)");
	exec(db, "INSERT INTO kind VALUES (0,'zero'),(1,'one'),(2,'two'),(3,'three'),(4,'four'),"
		 "(5,'five'),(6,'six')");
	check(integer(db, "SELECT count(*) FROM item JOIN kind ON item.kind = kind.id "
			  "WHERE label LIKE 't%'") == 5714, "join");
	check(integer(db, "SELECT count(*) FROM item WHERE name GLOB 'item-0000?'") == 10, "glob");

	/* Functions: dates, JSON, maths and text. */
	check(text_is(db, "SELECT date('2026-02-28', '+1 day')", "2026-03-01"), "date arithmetic");
	check(integer(db, "SELECT CAST(julianday('2000-01-01') AS INT)") == 2451544, "julian day");
	check(integer(db, "SELECT unixepoch('now') >= 0"), "current time");
	check(text_is(db, "SELECT json_extract('{\"a\":[1,2,{\"b\":\"wyrm\"}]}', '$.a[2].b')", "wyrm"),
	      "json");
	check(text_is(db, "SELECT printf('%.5f', pi())", "3.14159"), "maths functions");
	check(text_is(db, "SELECT upper('wyrmroot') || length('🐉')", "WYRMROOT1"), "text and UTF-8");

	/* A rolled-back transaction leaves the table as it was. */
	exec(db, "BEGIN; DELETE FROM item WHERE kind = 3; UPDATE item SET price = 0");
	check(integer(db, "SELECT count(*) FROM item") == 20000 - 2857, "delete in transaction");
	exec(db, "ROLLBACK");
	check(integer(db, "SELECT count(*) FROM item WHERE price > 0") == 20000, "rollback");

	/* Large values and the pseudo-random source. */
	exec(db, "CREATE TABLE blob(data BLOB)");
	exec(db, "INSERT INTO blob VALUES (randomblob(1000000)), (zeroblob(2000000))");
	check(integer(db, "SELECT sum(length(data)) FROM blob") == 3000000, "blobs");
	check(integer(db, "SELECT count(DISTINCT hex(randomblob(8))) FROM item LIMIT 1") == 20000,
	      "random values");

	/* A second connection is refused the write lock while the first holds
	 * it, and gets it once the first commits. */
	exec(db, "BEGIN IMMEDIATE");
	check(access(LOCK, F_OK) == 0, "lock directory while locked");
	sqlite3 *other = open_database();
	check(sqlite3_exec(other, "BEGIN IMMEDIATE", 0, 0, 0) == SQLITE_BUSY, "second writer is busy");
	exec(db, "INSERT INTO kind VALUES (7, 'seven')");
	exec(db, "COMMIT");
	check(access(LOCK, F_OK) != 0 && errno == ENOENT, "lock released");
	exec(other, "BEGIN IMMEDIATE");
	check(integer(other, "SELECT label = 'seven' FROM kind WHERE id = 7"), "second writer sees commit");
	exec(other, "DELETE FROM blob; COMMIT");
	sqlite3_close(other);

	/* Shrinking the file, then checking it. */
	sqlite3_int64 pages = integer(db, "PRAGMA page_count");
	exec(db, "VACUUM");
	check(integer(db, "PRAGMA page_count") < pages, "vacuum shrinks the file");
	check(text_is(db, "PRAGMA integrity_check", "ok"), "integrity check");
	check(sqlite3_close(db) == SQLITE_OK, "close");

	/* What was written is there after reopening. */
	db = open_database();
	check(integer(db, "SELECT count(*) FROM item") == 20000, "rows persist");
	check(text_is(db, "SELECT label FROM kind WHERE id = 7", "seven"), "commit persists");
	sqlite3_close(db);
	check(unlink(PATH) == 0, "remove the database");
	check(access("/sqlite-test.db-journal", F_OK) != 0, "no journal left");

	printf("sqlite tests passed: %d checks, SQLite %s\n", checks, sqlite3_libversion());
	return 0;
}
