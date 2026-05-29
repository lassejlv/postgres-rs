-- Introspection queries of the kind ORMs run against information_schema to
-- discover the schema. Each runs read-only after orm_schema.sql is loaded.

-- List the user tables (Prisma/Drizzle enumerate base tables in 'public').
SELECT table_name FROM information_schema.tables
WHERE table_schema = 'public' AND table_type = 'BASE TABLE'
ORDER BY table_name;

-- Columns of a table, in declaration order.
SELECT column_name, data_type, is_nullable
FROM information_schema.columns
WHERE table_name = 'User'
ORDER BY ordinal_position;

-- Constraint types declared on a table.
SELECT constraint_type
FROM information_schema.table_constraints
WHERE table_name = 'Post'
ORDER BY constraint_type;
