DROP VIEW "open_orders";
DROP INDEX "orders_by_reference";
ALTER TABLE "orders" DROP COLUMN "reference";
DROP TABLE "shipments";
