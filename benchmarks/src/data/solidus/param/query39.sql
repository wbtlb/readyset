SELECT COUNT(count_column) FROM (SELECT DISTINCT `spree_products`.`id` AS count_column FROM `spree_products` INNER JOIN `spree_variants` ON `spree_variants`.`is_master` = TRUE AND `spree_variants`.`product_id` = `spree_products`.`id` INNER JOIN `spree_variants` `variants_including_masters_spree_products_join` ON `variants_including_masters_spree_products_join`.`deleted_at` IS NULL AND `variants_including_masters_spree_products_join`.`product_id` = `spree_products`.`id` INNER JOIN `spree_prices` ON `spree_prices`.`deleted_at` IS NULL AND `spree_prices`.`variant_id` = `variants_including_masters_spree_products_join`.`id` WHERE `spree_products`.`deleted_at` IS NULL AND EXISTS (SELECT `spree_prices`.* FROM `spree_prices` WHERE `spree_prices`.`deleted_at` IS NULL AND `spree_variants`.`id` = `spree_prices`.`variant_id`) AND (`spree_products`.available_on <= '2022-02-28 00:51:25.575848') AND (`spree_products`.discontinue_on IS NULL OR`spree_products`.discontinue_on >= '2022-02-28 00:51:25.576089') AND `spree_prices`.`deleted_at` IS NULL AND `spree_prices`.`currency` = 'USD' AND `spree_prices`.`country_iso` IS NULL LIMIT 12 OFFSET 0) subquery_for_count;
