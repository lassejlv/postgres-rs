-- Representative schema in the shape ORMs like Prisma / Drizzle emit:
-- serial primary keys, a UNIQUE column, timestamps, and a foreign key.
-- Quoted PascalCase identifiers mirror Prisma's default model->table mapping.

CREATE TABLE "User" (
    id serial PRIMARY KEY,
    email text NOT NULL UNIQUE,
    name text,
    created_at timestamp NOT NULL
);

CREATE TABLE "Post" (
    id serial PRIMARY KEY,
    title text NOT NULL,
    published boolean NOT NULL DEFAULT false,
    author_id integer NOT NULL,
    created_at timestamp NOT NULL,
    FOREIGN KEY (author_id) REFERENCES "User"(id)
);

INSERT INTO "User" (email, name, created_at) VALUES
    ('alice@example.com', 'Alice', '2024-01-01 10:00:00'),
    ('bob@example.com', 'Bob', '2024-01-02 11:00:00');

INSERT INTO "Post" (title, published, author_id, created_at) VALUES
    ('Hello World', true, 1, '2024-01-03 09:00:00'),
    ('Draft', false, 1, '2024-01-04 09:00:00'),
    ('Bob''s first', true, 2, '2024-01-05 09:00:00');
