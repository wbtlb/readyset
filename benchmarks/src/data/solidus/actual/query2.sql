SELECT `spree_assets`.* FROM `spree_assets` INNER JOIN `spree_variants` ON `spree_assets`.`viewable_id` = `spree_variants`.`id` WHERE `spree_variants`.`deleted_at` IS NULL AND `spree_assets`.`type` = 'Spree::Image' AND `spree_variants`.`product_id` = 1 AND `spree_assets`.`viewable_type` = 'Spree::Variant' ORDER BY `spree_assets`.`position` ASC, `spree_variants`.`position` ASC LIMIT 1;
